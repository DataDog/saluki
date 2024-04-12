package main

import (
	"log"
	"os"
	"strconv"
	"strings"

	"github.com/DataDog/datadog-go/v5/statsd"
)

func main() {
	args := os.Args[1:]
	if len(args) < 2 {
		log.Fatal("Usage: dogstatsd_client <addr> <operation>")
	}

	addr := args[0]
	operation := args[1]

	// Create our client.
	client, err := statsd.New(addr, statsd.WithOriginDetection(), statsd.WithoutTelemetry(), statsd.WithExtendedClientSideAggregation())
	if err != nil {
		log.Fatal(err)
	}

	// Parse the operation, which could be multiple operations.
	operations := strings.Split(operation, ",")
	for _, op := range operations {
		parts := strings.Split(op, ":")
		if len(parts) != 2 {
			log.Fatalf("Invalid operation '%s'", op)
		}

		metric_type := parts[0]

		if metric_type == "set" {
			client.Set("dsd_client.test.set", parts[1], []string{"source:dogstatsd-client"}, 1)
		} else {
			metric_value, err := strconv.ParseFloat(strings.TrimSpace(parts[1]), 64)
			if err != nil {
				log.Fatalf("Invalid metric value '%s'", parts[1])
			}

			switch metric_type {
			case "count":
				client.Incr("dsd_client.test.counter", []string{"source:dogstatsd-client"}, metric_value)
			case "gauge":
				client.Gauge("dsd_client.test.gauge", metric_value, []string{"source:dogstatsd-client"}, 1)
			case "histogram":
				client.Histogram("dsd_client.test.histogram", metric_value, []string{"source:dogstatsd-client"}, 1)
			case "distribution":
				client.Distribution("dsd_client.test.distribution", metric_value, []string{"source:dogstatsd-client"}, 1)
			default:
				log.Fatalf("Invalid metric type '%s'", metric_type)
			}
		}
	}

	if err := client.Close(); err != nil {
		log.Fatal(err)
	}
}
