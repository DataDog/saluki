#!/usr/bin/env python3

import json
import sys


def sizeof_fmt(num):
	for unit in ('B', 'KB', 'MB', 'GB', 'TB'):
		if num < 1024.0:
			if unit == 'B':
				return '{:3} {}'.format(int(num), unit)
			else:
				return '{:3.1f} {}'.format(num, unit)
		num /= 1024.0

def calculate_arena_stats(arena_config, arena_id, arena_stats):
	print(f"Arena '{arena_id}' statistics:")

	total_resident_size = 0
	total_wasted_size = 0
	
	(total_small_resident_size, total_small_wasted_size) = calculate_bin_stats(arena_config, arena_stats["bins"])
	total_resident_size += total_small_resident_size
	total_wasted_size += total_small_wasted_size

	print("")

	total_large_resident_size = calculate_lextent_stats(arena_config, arena_stats["lextents"])
	total_resident_size += total_large_resident_size

	print("")
	print(f"  Arena resident size: {sizeof_fmt(total_resident_size)}")
	print(f"  Arena wasted size: {sizeof_fmt(total_wasted_size)}")
	print("")
	print("")


def calculate_bin_stats(arena_config, arena_bin_stats):
	BIN_FMT = '  {0:>3} {1:>11} {2:>9} {3:>12} {4:>14} {5:>13} {6:>14} {7:>6} {8:>11}'

	arena_bins = arena_config["bin"]
	total_resident_size = 0
	total_wasted_size = 0

	print(BIN_FMT.format('bin', 'region size', 'slab size', 'active slabs', 'active regions', 'resident size', 'allocated size', 'wasted', 'wasted size'))
	print(BIN_FMT.format('---', '-----------', '---------', '------------', '--------------', '-------------', '--------------', '------', '-----------'))
	print("")

	for bin_id, bin_stats in enumerate(arena_bin_stats):
		bin_slab_size = arena_bins[bin_id]["slab_size"]
		bin_region_size = arena_bins[bin_id]["size"]
		bin_active_slabs = bin_stats["curslabs"]
		bin_active_regions = bin_stats["curregs"]

		bin_overall_size = bin_slab_size * bin_active_slabs
		bin_active_size = bin_region_size * bin_active_regions
		bin_wasted_size = 0
		if bin_overall_size != 0:
			bin_wasted_size = bin_overall_size - bin_active_size
		bin_wasted_pct = 0.0
		if bin_overall_size != 0:
			bin_wasted_pct = 100 * bin_wasted_size / bin_overall_size

		total_resident_size += bin_overall_size
		total_wasted_size += bin_overall_size - bin_active_size

		print(BIN_FMT.format(bin_id, sizeof_fmt(bin_region_size), sizeof_fmt(bin_slab_size), 
			bin_active_slabs, bin_active_regions, sizeof_fmt(bin_overall_size),
			sizeof_fmt(bin_active_size), "{0:>5.1f}%".format(bin_wasted_pct),
			sizeof_fmt(bin_wasted_size)))
	
	return (total_resident_size, total_wasted_size)


def calculate_lextent_stats(arena_config, arena_lextent_stats):
	EXTENT_FMT = '  {0:>6} {1:>11} {2:>14} {3:>13}'

	arena_extents = arena_config["lextent"]
	total_resident_size = 0

	print(EXTENT_FMT.format('extent', 'extent size', 'active extents', 'resident size'))
	print(EXTENT_FMT.format('------', '-----------', '--------------', '-------------'))
	print("")

	for extent_id, extent_stats in enumerate(arena_lextent_stats):
		extent_size = arena_extents[extent_id]["size"]
		active_extents = extent_stats["curlextents"]
		if active_extents == 0:
			continue

		extent_overall_size = extent_size * active_extents

		total_resident_size += extent_overall_size

		print(EXTENT_FMT.format(extent_id, sizeof_fmt(extent_size), active_extents, sizeof_fmt(extent_overall_size)))

	return total_resident_size


def main():
	if len(sys.argv) < 2:
		print("Usage: {0} <jemalloc JSON stats>".format(sys.argv[0]), file=sys.stderr)
		return 1
	 
	# Parse the 2nd argument as a JSON file.
	with open(sys.argv[1]) as f:
		jemalloc_stats = json.load(f)
		arena_config = jemalloc_stats["jemalloc"]["arenas"]
		merged_arena_stats = jemalloc_stats["jemalloc"]["stats.arenas"]["merged"]
		calculate_arena_stats(arena_config, "merged", merged_arena_stats)

		print("Overall statistics:\n  Total bytes allocated: {0}\n  Total bytes resident: {1}\n  Total bytes metadata: {2}".format(
			sizeof_fmt(jemalloc_stats["jemalloc"]["stats"]["allocated"]),
			sizeof_fmt(jemalloc_stats["jemalloc"]["stats"]["resident"]),
			sizeof_fmt(jemalloc_stats["jemalloc"]["stats"]["metadata"]),
		))

if __name__ == '__main__':
	main()
