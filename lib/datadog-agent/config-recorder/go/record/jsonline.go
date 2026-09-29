// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package record

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"sort"
	"strconv"
)

// jsonObject is one object of a line being built. Members are written sorted by key in byte
// order. A member is left out by not adding it; a nil value is written as `null`.
type jsonObject map[string]interface{}

// Line is one corpus line: a header, case or key line.
type Line interface {
	// Validate checks the line against the record format's consistency rules.
	Validate() error
	object() (jsonObject, error)
}

// MarshalLine validates a line and returns its canonical bytes, without the trailing newline.
func MarshalLine(l Line) ([]byte, error) {
	if err := l.Validate(); err != nil {
		return nil, err
	}
	obj, err := l.object()
	if err != nil {
		return nil, err
	}
	var buf bytes.Buffer
	if err := writeCanonical(&buf, obj); err != nil {
		return nil, err
	}
	return buf.Bytes(), nil
}

// WriteLine writes one canonical line followed by a newline.
func WriteLine(w io.Writer, l Line) error {
	b, err := MarshalLine(l)
	if err != nil {
		return err
	}
	b = append(b, '\n')
	_, err = w.Write(b)
	return err
}

// writeCanonical writes v with no insignificant whitespace and object members sorted. A
// json.RawMessage is written verbatim: it is either an encoder's output or protojson bytes whose
// own escaping must survive.
func writeCanonical(buf *bytes.Buffer, v interface{}) error {
	switch v := v.(type) {
	case nil:
		buf.WriteString("null")
	case json.RawMessage:
		var compact bytes.Buffer
		if err := json.Compact(&compact, v); err != nil {
			return fmt.Errorf("embedded JSON is not valid: %q: %w", v, err)
		}
		if !bytes.Equal(compact.Bytes(), v) {
			return fmt.Errorf("embedded JSON is not compact: %q", v)
		}
		buf.Write(v)
	case string:
		return encodeString(buf, v)
	case bool:
		buf.WriteString(strconv.FormatBool(v))
	case int:
		buf.WriteString(strconv.Itoa(v))
	case int64:
		buf.WriteString(strconv.FormatInt(v, 10))
	case uint64:
		buf.WriteString(strconv.FormatUint(v, 10))
	case []string:
		buf.WriteByte('[')
		for i, s := range v {
			if i > 0 {
				buf.WriteByte(',')
			}
			if err := encodeString(buf, s); err != nil {
				return err
			}
		}
		buf.WriteByte(']')
	case []interface{}:
		buf.WriteByte('[')
		for i, e := range v {
			if i > 0 {
				buf.WriteByte(',')
			}
			if err := writeCanonical(buf, e); err != nil {
				return err
			}
		}
		buf.WriteByte(']')
	case jsonObject:
		keys := make([]string, 0, len(v))
		for k := range v {
			keys = append(keys, k)
		}
		sort.Strings(keys)
		buf.WriteByte('{')
		for i, k := range keys {
			if i > 0 {
				buf.WriteByte(',')
			}
			if err := encodeString(buf, k); err != nil {
				return err
			}
			buf.WriteByte(':')
			if err := writeCanonical(buf, v[k]); err != nil {
				return err
			}
		}
		buf.WriteByte('}')
	default:
		return fmt.Errorf("line writer: unsupported member type %T", v)
	}
	return nil
}
