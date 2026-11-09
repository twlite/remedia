/** Native process memory accounting in bytes. Missing fields indicate an older server; null indicates unavailable data. */
export interface ResourceMemory {
  /** Resident bytes including shared pages. */
  residentBytes?: number | null;
  /** Private resident bytes (USS); excludes shared pages, distinct from private commit. */
  privateResidentBytes?: number | null;
  /** Proportional resident bytes (PSS), including shared pages proportionally. */
  proportionalResidentBytes?: number | null;
  /** Private committed bytes, including potentially nonresident pages. */
  committedPrivateBytes?: number | null;
  /** Mapped virtual address bytes; not physical RAM. */
  virtualBytes?: number | null;
  /** Reserved but uncommitted virtual address bytes; not RAM. */
  reservedVirtualBytes?: number | null;
  /** Lifetime peak resident bytes. */
  peakResidentBytes?: number | null;
  /** macOS physical footprint bytes; distinct from USS and RSS. */
  footprintBytes?: number | null;
}

/** Native process CPU accounting. Missing fields indicate an older server; null indicates unavailable data. */
export interface ResourceCpu {
  /** Process utilization: 100 means one logical CPU; can exceed 100. Null on the first sample. */
  processPercent?: number | null;
  /** Cumulative process user CPU time in milliseconds. */
  userTimeMs?: number | null;
  /** Cumulative process system CPU time in milliseconds. */
  systemTimeMs?: number | null;
  /** Available logical CPU parallelism estimate. */
  availableParallelism?: number | null;
  /** Actual process OS thread count, including nonaudio threads. */
  threadCount?: number | null;
  /** System one-minute load average; not process CPU usage. */
  loadAverage1m?: number | null;
}

/** Cached native measurements for the whole host process, shared by all embedded instances. */
export interface ProcessResourceStatistics {
  /** Process memory counters in bytes. */
  memory: ResourceMemory;
  /** Process CPU and system load counters. */
  cpu: ResourceCpu;
  /** Collection completion timestamp as Unix milliseconds; null before the first collection. */
  collectedAtUnixMs: number | null;
  /** Wall interval used for process utilization in milliseconds; null on the first sample. */
  cpuSampleIntervalMs: number | null;
  /** Demand-driven cache refresh interval in milliseconds; concurrent requests share stale snapshots. */
  samplingIntervalMs: number;
  /** Native collection duration in milliseconds, or null before collection. */
  collectionDurationMs: number | null;
  /** Unix descriptors or Windows handles owned by the process, or null if unavailable. */
  openHandles: number | null;
}
