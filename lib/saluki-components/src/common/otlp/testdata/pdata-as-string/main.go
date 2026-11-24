// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).

// Command pdata-as-string writes the test vectors of `otlp_value_as_string` in Saluki.
//
// Each vector holds an OTLP logs export request whose only log record has the vector's value as its body, encoded as
// protobuf and then base64, and the `pcommon.Value.AsString` of that body. The expected string is taken from the body
// after the request is decoded again, so it describes exactly the bytes that the Rust test decodes.
//
// Run `make build-otlp-as-string-vectors` to regenerate the vectors.
package main

import (
	"bytes"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"math"
	"math/rand/v2"
	"os"
	"runtime/debug"

	"go.opentelemetry.io/collector/pdata/pcommon"
	"go.opentelemetry.io/collector/pdata/plog"
	"go.opentelemetry.io/collector/pdata/plog/plogotlp"
)

const pdataModule = "go.opentelemetry.io/collector/pdata"

type vector struct {
	Name     string `json:"name"`
	Request  string `json:"request"`
	AsString string `json:"as_string"`
}

type output struct {
	PdataVersion string   `json:"pdata_version"`
	Vectors      []vector `json:"vectors"`
}

// A case fills in the body of a log record.
type fill func(body pcommon.Value)

// Keys that the duplicate-key cases rename after encoding, so that a map holds the same key twice. pdata's API never
// creates duplicate keys, but a decoded request can hold them.
const (
	duplicateKey = "dupA"
	renamedKey   = "dupB"
)

func main() {
	var vectors []vector
	add := func(name string, f fill) {
		vectors = append(vectors, newVector(name, f, false))
	}
	addWithDuplicate := func(name string, f fill) {
		vectors = append(vectors, newVector(name, f, true))
	}

	// Scalars, written as `AsString` writes them outside JSON.
	add("empty", func(v pcommon.Value) {})
	add("string", func(v pcommon.Value) { v.SetStr("plain <&> \u2028 \"text\"") })
	add("empty string", func(v pcommon.Value) { v.SetStr("") })
	add("true", func(v pcommon.Value) { v.SetBool(true) })
	add("false", func(v pcommon.Value) { v.SetBool(false) })
	add("int min", func(v pcommon.Value) { v.SetInt(math.MinInt64) })
	add("int max", func(v pcommon.Value) { v.SetInt(math.MaxInt64) })
	add("bytes", func(v pcommon.Value) { v.SetEmptyBytes().FromRaw([]byte{0x00, 0xff, 0xfe, 'a'}) })
	add("empty bytes", func(v pcommon.Value) { v.SetEmptyBytes() })
	for _, d := range doubles() {
		d := d
		add(fmt.Sprintf("double %v (%016x)", d, math.Float64bits(d)), func(v pcommon.Value) { v.SetDouble(d) })
		add(fmt.Sprintf("double %v (%016x) in an array", d, math.Float64bits(d)), func(v pcommon.Value) {
			v.SetEmptySlice().AppendEmpty().SetDouble(d)
		})
	}

	// Doubles from random bits, to cover the float layout broadly. The seed is fixed, so the vectors are stable.
	rng := rand.New(rand.NewPCG(1, 2))
	for i := 0; i < 8; i++ {
		add(fmt.Sprintf("random doubles %d", i), func(v pcommon.Value) {
			s := v.SetEmptySlice()
			for len := 0; len < 32; {
				d := math.Float64frombits(rng.Uint64())
				if math.IsNaN(d) || math.IsInf(d, 0) {
					continue
				}
				s.AppendEmpty().SetDouble(d)
				len++
			}
		})
	}

	// Arrays and maps.
	add("empty array", func(v pcommon.Value) { v.SetEmptySlice() })
	add("empty map", func(v pcommon.Value) { v.SetEmptyMap() })
	add("array of every type", func(v pcommon.Value) {
		s := v.SetEmptySlice()
		s.AppendEmpty()
		s.AppendEmpty().SetStr("s")
		s.AppendEmpty().SetBool(true)
		s.AppendEmpty().SetInt(-1)
		s.AppendEmpty().SetDouble(1)
		s.AppendEmpty().SetEmptyBytes()
		s.AppendEmpty().SetEmptyBytes().FromRaw([]byte("hi"))
		s.AppendEmpty().SetEmptySlice()
		s.AppendEmpty().SetEmptyMap()
	})
	add("map of every type", func(v pcommon.Value) {
		m := v.SetEmptyMap()
		m.PutEmpty("empty")
		m.PutStr("string", "s")
		m.PutBool("bool", false)
		m.PutInt("int", math.MaxInt64)
		m.PutDouble("double", 2.5)
		m.PutDouble("negative zero", math.Copysign(0, -1))
		m.PutEmptyBytes("empty bytes")
		m.PutEmptyBytes("bytes").FromRaw([]byte("hi"))
		m.PutEmptySlice("array")
		m.PutEmptyMap("map")
	})
	add("map keys sort by their bytes", func(v pcommon.Value) {
		m := v.SetEmptyMap()
		for _, key := range []string{"z", "é", "a", "A", "_", "", "aa", "a.b", "😀", "ÿ", "Z", "1", "\u2028"} {
			m.PutStr(key, key)
		}
	})
	addWithDuplicate("the last duplicate key wins", func(v pcommon.Value) {
		m := v.SetEmptyMap()
		m.PutStr(duplicateKey, "first")
		m.PutStr("b", "middle")
		m.PutStr(renamedKey, "last")
	})
	addWithDuplicate("the last duplicate key wins across types", func(v pcommon.Value) {
		m := v.SetEmptyMap()
		m.PutEmptyMap(duplicateKey).PutStr("k", "v")
		m.PutInt(renamedKey, 2)
	})
	add("strings escape like encoding/json without HTML escaping", func(v pcommon.Value) {
		s := v.SetEmptySlice()
		var controls []byte
		for b := byte(0); b < 0x20; b++ {
			controls = append(controls, b)
		}
		s.AppendEmpty().SetStr(string(controls))
		s.AppendEmpty().SetStr("<script>&amp;</script>")
		s.AppendEmpty().SetStr("quote \" backslash \\ slash / DEL \x7f")
		s.AppendEmpty().SetStr("line \u2028 paragraph \u2029 next \u0085 nbsp \u00a0")
		s.AppendEmpty().SetStr("é 😀 � \ufeff")
		s.AppendEmpty().SetEmptyMap().PutStr("<key>&\"\n", "value")
	})
	add("deep nesting", func(v pcommon.Value) {
		inner := pcommon.NewValueStr("leaf")
		for i := 0; i < 20; i++ {
			next := pcommon.NewValueEmpty()
			if i%2 == 0 {
				inner.CopyTo(next.SetEmptyMap().PutEmpty(fmt.Sprintf("level%d", i)))
			} else {
				inner.CopyTo(next.SetEmptySlice().AppendEmpty())
			}
			inner = next
		}
		inner.CopyTo(v)
	})

	// Non-finite doubles make `json.Encoder` fail, so the whole string is empty.
	for _, nonFinite := range []struct {
		name  string
		value float64
	}{{"NaN", math.NaN()}, {"Infinity", math.Inf(1)}, {"-Infinity", math.Inf(-1)}} {
		name, d := nonFinite.name, nonFinite.value
		add(name+" in an array", func(v pcommon.Value) {
			s := v.SetEmptySlice()
			s.AppendEmpty().SetStr("before")
			s.AppendEmpty().SetDouble(d)
		})
		add(name+" deep in a map", func(v pcommon.Value) {
			m := v.SetEmptyMap()
			m.PutStr("a", "before")
			m.PutEmptySlice("b").AppendEmpty().SetEmptyMap().PutDouble("c", d)
		})
	}

	out := output{PdataVersion: pdataVersion(), Vectors: vectors}
	encoder := json.NewEncoder(os.Stdout)
	encoder.SetEscapeHTML(false)
	encoder.SetIndent("", "  ")
	if err := encoder.Encode(out); err != nil {
		fail("writing the vectors: %v", err)
	}
}

// doubles returns the doubles to check one by one: the layout thresholds and their neighbors, zeros, and extremes.
func doubles() []float64 {
	var values []float64
	for _, threshold := range []float64{1e-6, 1e21} {
		values = append(values,
			threshold,
			math.Nextafter(threshold, 0),
			math.Nextafter(threshold, math.Inf(1)),
			-threshold,
		)
	}
	return append(values,
		0,
		math.Copysign(0, -1),
		1,
		-1,
		1.5,
		0.1,
		1.0/3,
		100,
		1e20,
		123456789012345678,
		1e-7,
		1.5e-7,
		1e-10,
		1e100,
		1e-100,
		1.5e300,
		math.MaxFloat64,
		-math.MaxFloat64,
		math.SmallestNonzeroFloat64,
		2.2250738585072014e-308,
		float64(math.MaxInt64),
		9007199254740993,
	)
}

// newVector builds the request for a case, and renders the body of the decoded request.
func newVector(name string, f fill, duplicate bool) vector {
	logs := plog.NewLogs()
	record := logs.ResourceLogs().AppendEmpty().ScopeLogs().AppendEmpty().LogRecords().AppendEmpty()
	f(record.Body())

	request, err := plogotlp.NewExportRequestFromLogs(logs).MarshalProto()
	if err != nil {
		fail("%s: encoding the request: %v", name, err)
	}
	if duplicate {
		if bytes.Count(request, []byte(renamedKey)) != 1 {
			fail("%s: the request must hold %q exactly once", name, renamedKey)
		}
		request = bytes.ReplaceAll(request, []byte(renamedKey), []byte(duplicateKey))
	}

	decoded := plogotlp.NewExportRequest()
	if err := decoded.UnmarshalProto(request); err != nil {
		fail("%s: decoding the request: %v", name, err)
	}
	body := decoded.Logs().ResourceLogs().At(0).ScopeLogs().At(0).LogRecords().At(0).Body()
	return vector{
		Name:     name,
		Request:  base64.StdEncoding.EncodeToString(request),
		AsString: body.AsString(),
	}
}

// pdataVersion returns the version of pdata that this command is built with.
func pdataVersion() string {
	info, ok := debug.ReadBuildInfo()
	if !ok {
		fail("reading the build info")
	}
	for _, dep := range info.Deps {
		if dep.Path == pdataModule {
			return dep.Version
		}
	}
	fail("finding %s in the build info", pdataModule)
	return ""
}

func fail(format string, args ...any) {
	fmt.Fprintf(os.Stderr, "pdata-as-string: "+format+"\n", args...)
	os.Exit(1)
}
