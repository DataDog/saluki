// Oracle for Saluki's trace decoder fixtures: it decodes each case's body with the trace-agent's own receiver code
// and dumps what that code produced, so that Saluki's decoders can be tested against it.
//
// This file is not built with Saluki. regenerate.sh overlays it into the datadog-agent checkout's pkg/trace/api
// package with `go test -overlay`, which leaves the checkout untouched. See docs/development/testing.md.
//
// Input (ORACLE_IN): a JSON list of cases, each {name, content_type?, body | body_b64, version?}. Output
// (ORACLE_OUT): one result per case, {name, media_type, payload} or {name, media_type, error: {tag, reason, ...}}.

package api

import (
	"bytes"
	"context"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"testing"

	"github.com/DataDog/datadog-agent/pkg/proto/pbgo/trace/idx"
	"github.com/DataDog/datadog-agent/pkg/trace/api/apiutil"
	"github.com/tinylib/msgp/msgp"
)

type oracleCase struct {
	Name        string  `json:"name"`
	ContentType *string `json:"content_type"`
	Body        string  `json:"body"`
	BodyB64     string  `json:"body_b64"`
	Version     string  `json:"version"`
}

type stubCID struct{}

func (stubCID) GetContainerID(context.Context, http.Header) string { return "" }

func dumpAny(s *idx.StringTable, v *idx.AnyValue) any {
	switch x := v.Value.(type) {
	case *idx.AnyValue_StringValueRef:
		return map[string]any{"string": s.Get(x.StringValueRef)}
	case *idx.AnyValue_BoolValue:
		return map[string]any{"bool": x.BoolValue}
	case *idx.AnyValue_DoubleValue:
		return map[string]any{"double": fmt.Sprintf("%v", x.DoubleValue)}
	case *idx.AnyValue_IntValue:
		return map[string]any{"int": x.IntValue}
	case *idx.AnyValue_BytesValue:
		return map[string]any{"bytes": base64.StdEncoding.EncodeToString(x.BytesValue)}
	case *idx.AnyValue_ArrayValue:
		out := []any{}
		for _, e := range x.ArrayValue.Values {
			out = append(out, dumpAny(s, e))
		}
		return map[string]any{"array": out}
	}
	return map[string]any{"unknown": fmt.Sprintf("%T", v.Value)}
}

func dumpAttrs(s *idx.StringTable, m map[uint32]*idx.AnyValue) map[string]any {
	out := map[string]any{}
	for k, v := range m {
		out[s.Get(k)] = dumpAny(s, v)
	}
	return out
}

func dumpPayload(tp *idx.InternalTracerPayload) any {
	s := tp.Strings
	chunks := []any{}
	for _, c := range tp.Chunks {
		spans := []any{}
		for _, sp := range c.Spans {
			links := []any{}
			for _, l := range sp.Links() {
				links = append(links, map[string]any{"trace_id": hex.EncodeToString(l.TraceID()), "span_id": l.SpanID(), "flags": l.Flags(), "tracestate": l.Tracestate(), "attributes": dumpAttrs(s, l.Attributes())})
			}
			events := []any{}
			for _, e := range sp.Events() {
				events = append(events, map[string]any{"time": e.Time(), "name": e.Name(), "attributes": dumpAttrs(s, e.Attributes())})
			}
			spans = append(spans, map[string]any{
				"service": sp.Service(), "name": sp.Name(), "resource": sp.Resource(), "type": sp.Type(),
				"span_id": sp.SpanID(), "parent_id": sp.ParentID(), "start": sp.Start(), "duration": sp.Duration(),
				"error": sp.Error(), "env": sp.Env(), "version": sp.Version(), "component": sp.Component(),
				"kind": int(sp.Kind()), "attributes": dumpAttrs(s, sp.Attributes()), "links": links, "events": events,
			})
		}
		chunks = append(chunks, map[string]any{
			"priority": c.Priority, "origin": c.Origin(), "sampling_mechanism": c.SamplingMechanism(),
			"dropped_trace": c.DroppedTrace, "trace_id": hex.EncodeToString(c.TraceID),
			"attributes": dumpAttrs(s, c.Attributes), "spans": spans,
		})
	}
	return map[string]any{
		"env": tp.Env(), "hostname": tp.Hostname(), "app_version": tp.AppVersion(),
		"language_name": tp.LanguageName(), "language_version": tp.LanguageVersion(), "tracer_version": tp.TracerVersion(),
		"container_id": tp.ContainerID(), "runtime_id": tp.RuntimeID(),
		"attributes": dumpAttrs(s, tp.Attributes), "chunks": chunks,
	}
}

func classify(err error) map[string]any {
	errtag, reason := "decoding-error", "decoding_error"
	switch err {
	case apiutil.ErrLimitedReaderLimitReached:
		errtag, reason = "payload-too-large", "payload_too_large"
	case io.EOF, io.ErrUnexpectedEOF:
		errtag, reason = "unexpected-eof", "unexpected_eof"
	case msgp.ErrShortBytes:
		reason = "msgp_short_bytes"
	}
	if e, ok := err.(net.Error); ok && e.Timeout() {
		errtag, reason = "timeout", "timeout"
	}
	return map[string]any{"tag": errtag, "reason": reason, "message": err.Error(), "type": fmt.Sprintf("%T", err)}
}

func TestOracle(t *testing.T) {
	in := os.Getenv("ORACLE_IN")
	if in == "" {
		t.Skip()
	}
	raw, err := os.ReadFile(in)
	if err != nil {
		t.Fatal(err)
	}
	var cases []oracleCase
	if err := json.Unmarshal(raw, &cases); err != nil {
		t.Fatal(err)
	}
	r := newTestReceiverFromConfig(newTestReceiverConfig())
	results := []any{}
	for _, c := range cases {
		body := []byte(c.Body)
		if c.BodyB64 != "" {
			body, _ = base64.StdEncoding.DecodeString(c.BodyB64)
		}
		req := httptest.NewRequest("POST", "/v0.4/traces", bytes.NewReader(body))
		if c.ContentType != nil {
			req.Header.Set("Content-Type", *c.ContentType)
		}
		v := v04
		switch c.Version {
		case "v0.1":
			v = v01
		case "v0.2":
			v = v02
		case "v0.3":
			v = v03
		}
		res := map[string]any{"name": c.Name, "media_type": getMediaType(req)}
		tp, err := r.decodeConvertedTracerPayload(v, req, stubCID{}, "", "", "")
		if err != nil {
			res["error"] = classify(err)
		} else {
			res["payload"] = dumpPayload(tp)
		}
		results = append(results, res)
	}
	out, err := json.MarshalIndent(results, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(os.Getenv("ORACLE_OUT"), out, 0o644); err != nil {
		t.Fatal(err)
	}
}
