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

	"github.com/DataDog/datadog-agent/cmd/config-recorder/corpus"
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
func (c *logCapture) take() []corpus.Warning {
	pkglog.Flush()
	c.mu.Lock()
	defer c.mu.Unlock()
	var out []corpus.Warning
	for _, r := range c.records {
		if w, ok := corpus.WarningFromRecord(r); ok {
			out = append(out, w)
		}
	}
	c.records = nil
	return out
}

// installLogCapture routes the Agent's global logger into a new capture, then checks that a
// record is delivered before the logging call returns, without a flush. Per-call warning
// attribution relies on that.
func installLogCapture() (*logCapture, error) {
	c := &logCapture{}
	pkglog.SetupLogger(ddslog.NewWrapper(c), "debug")
	const probe = "config recorder: synchronous logging check"
	pkglog.Warn(probe)
	c.mu.Lock()
	n := len(c.records)
	got := n == 1 && c.records[0].Message == probe
	c.records = nil
	c.mu.Unlock()
	if !got {
		return nil, fmt.Errorf("the Agent logger did not deliver a record synchronously (%d records after one Warn); per-call warning attribution is not possible", n)
	}
	return c, nil
}

// errConstruction wraps an error the Agent returned while building the config.
type errConstruction struct{ err error }

func (e errConstruction) Error() string { return e.err.Error() }

// withFirstSnapshot builds the config by the Agent's fx path, subscribes to the config stream,
// waits for the first event, and calls fn with the config and the first snapshot.
//
// Before building the app it runs fx.ValidateApp on the same fx options, the way the Agent's own
// fxutil.TestOneShot does (pkg/util/fxutil/test.go): a wiring error there (a missing or ambiguous
// dependency in the fx graph) is returned as a plain error, a harness failure. Only an error from
// running the validated graph before fn is entered -- from the config's own construction -- is
// returned as errConstruction, a startup error (record.md §4.1).
func withFirstSnapshot(params config.Params, fn func(config.Component, *pb.ConfigSnapshot) error) error {
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
		return fn(cfg, snapshot)
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
