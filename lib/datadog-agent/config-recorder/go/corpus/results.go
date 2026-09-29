// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package corpus

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"math"
	"reflect"
	"sort"
	"strconv"
	"unicode/utf8"
)

// ErrResultEncoding marks a getter result the record format cannot represent.
var ErrResultEncoding = errors.New("cannot encode getter result")

// EncodeResult writes a Go value by its dynamic type, recursively. It never goes through float64 or
// encoding/json's generic path, so integers stay exact and stay distinct from floats at any depth.
func EncodeResult(v interface{}) (json.RawMessage, error) {
	var buf bytes.Buffer
	if err := encodeValue(&buf, reflect.ValueOf(v)); err != nil {
		return nil, err
	}
	return buf.Bytes(), nil
}

func encodeValue(buf *bytes.Buffer, v reflect.Value) error {
	if !v.IsValid() {
		buf.WriteString("null")
		return nil
	}
	switch v.Kind() {
	case reflect.Interface:
		if v.IsNil() {
			buf.WriteString("null")
			return nil
		}
		return encodeValue(buf, v.Elem())
	case reflect.Bool:
		buf.WriteString(strconv.FormatBool(v.Bool()))
	case reflect.Int, reflect.Int8, reflect.Int16, reflect.Int32, reflect.Int64:
		// time.Duration is an int64, so this writes its nanoseconds.
		buf.WriteString(strconv.FormatInt(v.Int(), 10))
	case reflect.Uint, reflect.Uint8, reflect.Uint16, reflect.Uint32, reflect.Uint64, reflect.Uintptr:
		buf.WriteString(strconv.FormatUint(v.Uint(), 10))
	case reflect.Float32, reflect.Float64:
		return encodeFloat(buf, v.Float(), v.Type().Bits())
	case reflect.String:
		return encodeString(buf, v.String())
	case reflect.Slice, reflect.Array:
		if v.Type().Elem().Kind() == reflect.Uint8 {
			return fmt.Errorf("%w: unsupported type %s", ErrResultEncoding, v.Type())
		}
		if v.Kind() == reflect.Slice && v.IsNil() {
			buf.WriteString("null")
			return nil
		}
		buf.WriteByte('[')
		for i := 0; i < v.Len(); i++ {
			if i > 0 {
				buf.WriteByte(',')
			}
			if err := encodeValue(buf, v.Index(i)); err != nil {
				return err
			}
		}
		buf.WriteByte(']')
	case reflect.Map:
		return encodeMap(buf, v)
	default:
		return fmt.Errorf("%w: unsupported type %s", ErrResultEncoding, v.Type())
	}
	return nil
}

func encodeMap(buf *bytes.Buffer, v reflect.Value) error {
	var keyOf func(reflect.Value) string
	switch kt := v.Type().Key(); {
	case kt.Kind() == reflect.String:
		keyOf = func(k reflect.Value) string { return k.String() }
	case kt.Kind() == reflect.Interface && kt.NumMethod() == 0:
		// Same rendering as the config stream's sanitizeMapForJSON.
		keyOf = func(k reflect.Value) string { return fmt.Sprintf("%v", k.Interface()) }
	default:
		return fmt.Errorf("%w: unsupported type %s", ErrResultEncoding, v.Type())
	}
	if v.IsNil() {
		buf.WriteString("null")
		return nil
	}
	values := make(map[string]reflect.Value, v.Len())
	keys := make([]string, 0, v.Len())
	iter := v.MapRange()
	for iter.Next() {
		k := keyOf(iter.Key())
		if _, dup := values[k]; dup {
			return fmt.Errorf("%w: map keys collide as %q in %s", ErrResultEncoding, k, v.Type())
		}
		values[k] = iter.Value()
		keys = append(keys, k)
	}
	// A lone "$float" key would read back as a non-finite float.
	if len(keys) == 1 && keys[0] == "$float" {
		return fmt.Errorf("%w: map whose only key is \"$float\"", ErrResultEncoding)
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
		if err := encodeValue(buf, values[k]); err != nil {
			return err
		}
	}
	buf.WriteByte('}')
	return nil
}

func encodeFloat(buf *bytes.Buffer, f float64, bits int) error {
	switch {
	case math.IsNaN(f):
		buf.WriteString(`{"$float":"NaN"}`)
		return nil
	case math.IsInf(f, 1):
		buf.WriteString(`{"$float":"+Inf"}`)
		return nil
	case math.IsInf(f, -1):
		buf.WriteString(`{"$float":"-Inf"}`)
		return nil
	}
	var b []byte
	var err error
	if bits == 32 {
		b, err = json.Marshal(float32(f))
	} else {
		b, err = json.Marshal(f)
	}
	if err != nil {
		return fmt.Errorf("%w: %v", ErrResultEncoding, err)
	}
	buf.Write(b)
	// Keeps a whole float from reading back as an integer.
	if !bytes.ContainsAny(b, ".eE") {
		buf.WriteString(".0")
	}
	return nil
}

// encodeString writes s as encoding/json does with SetEscapeHTML(false). Invalid UTF-8 is an
// error because encoding/json would replace it silently.
func encodeString(buf *bytes.Buffer, s string) error {
	if !utf8.ValidString(s) {
		return fmt.Errorf("%w: invalid UTF-8 in string %q", ErrResultEncoding, s)
	}
	var tmp bytes.Buffer
	enc := json.NewEncoder(&tmp)
	enc.SetEscapeHTML(false)
	if err := enc.Encode(s); err != nil {
		return err
	}
	buf.Write(bytes.TrimSuffix(tmp.Bytes(), []byte("\n")))
	return nil
}
