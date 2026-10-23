// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package agentcfg

import (
	"errors"
	"math"
	"testing"

	pb "github.com/DataDog/datadog-agent/pkg/proto/pbgo/core"
	"google.golang.org/protobuf/types/known/structpb"
)

func TestSettingFromProto(t *testing.T) {
	tests := []struct {
		name string
		v    *structpb.Value
		want string
	}{
		{"string", structpb.NewStringValue("hello"), `"hello"`},
		{
			"struct with nested list",
			structpb.NewStructValue(&structpb.Struct{Fields: map[string]*structpb.Value{
				"a": structpb.NewListValue(&structpb.ListValue{Values: []*structpb.Value{
					structpb.NewNumberValue(1),
					structpb.NewStringValue("x"),
				}}),
			}}),
			`{"a":[1,"x"]}`,
		},
		{"large number", structpb.NewNumberValue(1e300), `1e+300`},
	}
	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			cs := &pb.ConfigSetting{Source: "file", Value: tc.v}
			s, err := SettingFromProto(cs)
			if err != nil {
				t.Fatalf("SettingFromProto: %v", err)
			}
			if s.Source != "file" {
				t.Errorf("source: got %q", s.Source)
			}
			if string(s.Value) != tc.want {
				t.Errorf("value: got %s, want %s", s.Value, tc.want)
			}
		})
	}
}

func TestSettingFromProtoNil(t *testing.T) {
	s, err := SettingFromProto(&pb.ConfigSetting{Source: "default", UnsetSource: "remote-config"})
	if err != nil {
		t.Fatalf("SettingFromProto: %v", err)
	}
	if s.Value != nil || s.Source != "default" || s.UnsetSource != "remote-config" {
		t.Errorf("got %+v", s)
	}
}

// TestSettingFromProtoNaN checks that an injected non-finite NumberValue is rejected: protojson
// has no JSON form for NaN, so protojson.Marshal itself returns the error (SettingFromProto never
// reaches the round-trip check for this input).
func TestSettingFromProtoNaN(t *testing.T) {
	cs := &pb.ConfigSetting{Source: "file", Value: structpb.NewNumberValue(math.NaN())}
	_, err := SettingFromProto(cs)
	if !errors.Is(err, ErrSettingRoundTrip) {
		t.Fatalf("got %v, want ErrSettingRoundTrip", err)
	}
}
