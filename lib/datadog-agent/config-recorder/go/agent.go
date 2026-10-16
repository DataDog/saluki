// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package main

import (
	"context"
	"errors"
	"fmt"
	stdslog "log/slog"
	"regexp"
	"strings"
	"sync"
	"time"

	"go.uber.org/fx"

	"github.com/DataDog/datadog-agent/comp/core/config"
	configstream "github.com/DataDog/datadog-agent/comp/core/configstream/def"
	configstreamfx "github.com/DataDog/datadog-agent/comp/core/configstream/fx"
	delegatedauthnoopfx "github.com/DataDog/datadog-agent/comp/core/delegatedauth/fx-noop"
	logdef "github.com/DataDog/datadog-agent/comp/core/log/def"
	logimpl "github.com/DataDog/datadog-agent/comp/core/log/impl"
	secretsnoopfx "github.com/DataDog/datadog-agent/comp/core/secrets/fx-noop"
	telemetrynoopfx "github.com/DataDog/datadog-agent/comp/core/telemetry/fx-noop"
	pb "github.com/DataDog/datadog-agent/pkg/proto/pbgo/core"
	"github.com/DataDog/datadog-agent/pkg/util/fxutil"
	pkglog "github.com/DataDog/datadog-agent/pkg/util/log"
	ddslog "github.com/DataDog/datadog-agent/pkg/util/log/slog"

	"github.com/DataDog/datadog-agent/cmd/config-recorder/record"
)

// logCapture is an slog.Handler that keeps every record the Agent's logger delivers.
type logCapture struct {
	mu      sync.Mutex
	records []stdslog.Record
}

func (c *logCapture) Enabled(context.Context, stdslog.Level) bool { return true }

func (c *logCapture) Handle(_ context.Context, rec stdslog.Record) error {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.records = append(c.records, rec.Clone())
	return nil
}

func (c *logCapture) WithAttrs([]stdslog.Attr) stdslog.Handler { return c }
func (c *logCapture) WithGroup(string) stdslog.Handler         { return c }

// take flushes the Agent's logger and returns the warnings delivered so far, clearing them.
func (c *logCapture) take() []record.Warning {
	pkglog.Flush()
	c.mu.Lock()
	defer c.mu.Unlock()
	var out []record.Warning
	for _, r := range c.records {
		if w, ok := record.WarningFromRecord(r); ok {
			out = append(out, w)
		}
	}
	c.records = nil
	return out
}

// callerPrefix is the `<file>:<line> ` the Agent's logger puts before the message of a record it
// buffered before it was set up (pkg/util/log/log.go, addLogToBuffer).
var callerPrefix = regexp.MustCompile(`^\S+:[0-9]+ `)

// stripCallerPrefix removes the caller position the Agent's logger adds to a buffered record's
// message. The position names the Agent's build path, not its behavior, so without it the record
// reads as the same warning logged after setup does. Only the start of the message is examined.
func stripCallerPrefix(msg string) string {
	return strings.TrimPrefix(msg, callerPrefix.FindString(msg))
}

// installLogCapture routes the Agent's global logger into a new capture, then checks that a
// record is delivered before the logging call returns, without a flush. Per-call warning
// attribution relies on that.
//
// Records the Agent logged before this call (during package initialization, for example an env
// value its transform cannot parse) are buffered by the Agent's logger and delivered by
// SetupLogger itself, in the order they were logged, each with the caller position prefixed to
// its message. They stay in the capture, with that prefix removed, so the first take returns
// them with the construction warnings. The check counts only the records that arrive after its
// probe call, which must be exactly the probe, and removes only the probe.
func installLogCapture() (*logCapture, error) {
	c := &logCapture{}
	pkglog.SetupLogger(ddslog.NewWrapper(c), "debug")
	c.mu.Lock()
	start := len(c.records)
	for i := range c.records {
		c.records[i].Message = stripCallerPrefix(c.records[i].Message)
	}
	c.mu.Unlock()
	const probe = "config recorder: synchronous logging check"
	pkglog.Warn(probe)
	c.mu.Lock()
	after := c.records[start:]
	n := len(after)
	got := n == 1 && after[0].Message == probe
	c.records = c.records[:start]
	c.mu.Unlock()
	if !got {
		return nil, fmt.Errorf("the Agent logger did not deliver a record synchronously (%d records after one Warn); per-call warning attribution is not possible", n)
	}
	return c, nil
}

// errConstruction wraps an error the Agent returned while building the config.
type errConstruction struct{ err error }

func (e errConstruction) Error() string { return e.err.Error() }

// session is what a case run works with once the first snapshot has arrived: the config, the
// first snapshot, the one subscription's later events, and the first snapshot's sequence ID, from
// which every later event's relative sequence is taken.
type session struct {
	cfg      config.Component
	snapshot *pb.ConfigSnapshot
	events   <-chan *pb.ConfigEvent
	base     uint64
}

// withFirstSnapshot builds the config by the Agent's fx path, subscribes to the config stream,
// waits for the first event, and calls fn with a session holding the config, the first snapshot
// and the subscription's event channel, which stays open until fn returns.
//
// Before building the app it runs fx.ValidateApp on the same fx options, the way the Agent's own
// fxutil.TestOneShot does (pkg/util/fxutil/test.go): a wiring error there (a missing or ambiguous
// dependency in the fx graph) returns a plain error and fails the recorder. Only an error from
// running the validated graph before fn is entered -- from the config's own construction -- is
// returned as errConstruction, a startup error (record.md §4.1).
func withFirstSnapshot(params config.Params, fn func(*session) error) error {
	entered := false
	oneShotFunc := func(cfg config.Component, stream configstream.Component) error {
		entered = true
		events, unsubscribe := stream.Subscribe(&pb.ConfigStreamRequest{Name: "config-recorder"})
		defer unsubscribe()
		var snapshot *pb.ConfigSnapshot
		select {
		case ev := <-events:
			snapshot = ev.GetSnapshot()
		case <-time.After(10 * time.Second):
			return errors.New("timed out waiting for the first config stream event")
		}
		if snapshot == nil {
			return errors.New("first config stream event is not a snapshot")
		}
		if snapshot.GetSequenceId() < 0 {
			return fmt.Errorf("first snapshot has a negative sequence ID %d", snapshot.GetSequenceId())
		}
		return fn(&session{cfg: cfg, snapshot: snapshot, events: events, base: uint64(snapshot.GetSequenceId())})
	}
	opts := []fx.Option{
		fx.Supply(params),
		config.Module(),
		secretsnoopfx.Module(),
		delegatedauthnoopfx.Module(),
		telemetrynoopfx.Module(),
		fx.Provide(func() logdef.Component { return logimpl.NewTemporaryLoggerWithoutInit() }),
		configstreamfx.Module(),
	}
	validateOpts := append(append([]fx.Option{}, opts...), fxutil.FxAgentBase(), fx.Invoke(oneShotFunc))
	if err := fx.ValidateApp(validateOpts...); err != nil {
		return fmt.Errorf("fx wiring: %w", err)
	}
	err := fxutil.OneShot(oneShotFunc, opts...)
	if err != nil && !entered {
		return errConstruction{err}
	}
	return err
}
