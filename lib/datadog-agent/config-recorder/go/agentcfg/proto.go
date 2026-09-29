// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package agentcfg

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"

	pb "github.com/DataDog/datadog-agent/pkg/proto/pbgo/core"
	"google.golang.org/protobuf/encoding/protojson"
	"google.golang.org/protobuf/proto"
	"google.golang.org/protobuf/types/known/structpb"

	"github.com/DataDog/datadog-agent/cmd/config-recorder/record"
)

// ErrSettingRoundTrip marks a streamed setting whose protojson output did not survive the
// round-trip check record.md §5.1 requires.
var ErrSettingRoundTrip = errors.New("streamed setting: protojson round trip mismatch")

// SettingFromProto encodes one streamed pb.ConfigSetting as a Setting (record.md §5.1). When the
// proto value is set, it is protojson.Marshal'd, passed through json.Compact, and embedded as raw
// bytes with protojson's own escaping intact. SettingFromProto then checks that
// protojson.Unmarshal of that output, remarshaled with proto.MarshalOptions{Deterministic: true},
// gives the same bytes as the original value under the same options; a mismatch is an error.
func SettingFromProto(cs *pb.ConfigSetting) (record.Setting, error) {
	s := record.Setting{Source: cs.GetSource(), UnsetSource: cs.GetUnsetSource()}
	v := cs.GetValue()
	if v == nil {
		return s, nil
	}

	marshaled, err := protojson.Marshal(v)
	if err != nil {
		// A non-finite google.protobuf.Value.NumberValue (NaN, +Inf, -Inf) fails here: protojson
		// has no JSON form for it and returns an error rather than writing invalid JSON.
		return record.Setting{}, fmt.Errorf("%w: protojson.Marshal: %v", ErrSettingRoundTrip, err)
	}
	var compact bytes.Buffer
	if err := json.Compact(&compact, marshaled); err != nil {
		return record.Setting{}, fmt.Errorf("%w: json.Compact: %v", ErrSettingRoundTrip, err)
	}

	back := &structpb.Value{}
	if err := protojson.Unmarshal(compact.Bytes(), back); err != nil {
		return record.Setting{}, fmt.Errorf("%w: protojson.Unmarshal: %v", ErrSettingRoundTrip, err)
	}
	opts := proto.MarshalOptions{Deterministic: true}
	want, err := opts.Marshal(v)
	if err != nil {
		return record.Setting{}, fmt.Errorf("%w: marshal original: %v", ErrSettingRoundTrip, err)
	}
	got, err := opts.Marshal(back)
	if err != nil {
		return record.Setting{}, fmt.Errorf("%w: marshal round trip: %v", ErrSettingRoundTrip, err)
	}
	if !bytes.Equal(want, got) {
		return record.Setting{}, fmt.Errorf("%w: bytes differ", ErrSettingRoundTrip)
	}

	s.Value = json.RawMessage(compact.Bytes())
	return s, nil
}
