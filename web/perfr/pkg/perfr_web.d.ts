/* tslint:disable */
/* eslint-disable */
export function page_pid(): number;
export function tid_main(): number;
export function tid_worker_base(): number;
export class WebProfiler {
  free(): void;
  constructor(capacity: number);
  page_pid(): number;
  intern(name: string): number;
  name_process(name: string): void;
  name_thread(tid: number, name: string): void;
  begin(name: string, tid: number, now_ns: number): number;
  end(handle: number, now_ns: number): void;
  begin_async(name: string, tid: number, now_ns: number): number;
  end_async(handle: number, now_ns: number): void;
  span(name: string, tid: number, start_ns: number, end_ns: number): void;
  span_async(name: string, tid: number, start_ns: number, end_ns: number): void;
  mark(name: string, tid: number, now_ns: number): void;
  value(name: string, tid: number, value: number, now_ns: number): void;
  stop(now_ns: number): void;
  clear(): void;
  resync(): void;
  set_enabled(on: boolean): void;
  flush(): Uint8Array;
  snapshot(now_ns: number): Uint8Array;
  live(): number;
  dropped(): number;
  produced(): number;
  capacity(): number;
  capturing(): boolean;
}

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
  readonly memory: WebAssembly.Memory;
  readonly perfr_web_page_pid: () => number;
  readonly perfr_web_now_ns: () => bigint;
  readonly perfr_web_set_tid: (a: number) => void;
  readonly perfr_web_begin: (a: number, b: number) => bigint;
  readonly perfr_web_end: (a: bigint) => void;
  readonly perfr_web_begin_async: (a: number, b: number) => bigint;
  readonly perfr_web_end_async: (a: bigint) => void;
  readonly perfr_web_span: (a: number, b: number, c: bigint, d: bigint) => void;
  readonly perfr_web_span_async: (a: number, b: number, c: bigint, d: bigint) => void;
  readonly perfr_web_value: (a: number, b: number, c: number) => void;
  readonly perfr_web_mark: (a: number, b: number) => void;
  readonly perfr_web_frame: () => void;
  readonly perfr_web_name_thread: (a: number, b: number, c: number) => void;
  readonly perfr_web_name_process: (a: number, b: number) => void;
  readonly perfr_web_stop: () => void;
  readonly perfr_web_clear: () => void;
  readonly perfr_web_flush: (a: number) => number;
  readonly perfr_web_snapshot: (a: number) => number;
  readonly __wbg_webprofiler_free: (a: number, b: number) => void;
  readonly webprofiler_new: (a: number) => number;
  readonly webprofiler_page_pid: (a: number) => number;
  readonly webprofiler_intern: (a: number, b: number, c: number) => number;
  readonly webprofiler_name_process: (a: number, b: number, c: number) => void;
  readonly webprofiler_name_thread: (a: number, b: number, c: number, d: number) => void;
  readonly webprofiler_begin: (a: number, b: number, c: number, d: number, e: number) => number;
  readonly webprofiler_end: (a: number, b: number, c: number) => void;
  readonly webprofiler_begin_async: (a: number, b: number, c: number, d: number, e: number) => number;
  readonly webprofiler_end_async: (a: number, b: number, c: number) => void;
  readonly webprofiler_span: (a: number, b: number, c: number, d: number, e: number, f: number) => void;
  readonly webprofiler_span_async: (a: number, b: number, c: number, d: number, e: number, f: number) => void;
  readonly webprofiler_mark: (a: number, b: number, c: number, d: number, e: number) => void;
  readonly webprofiler_value: (a: number, b: number, c: number, d: number, e: number, f: number) => void;
  readonly webprofiler_stop: (a: number, b: number) => void;
  readonly webprofiler_clear: (a: number) => void;
  readonly webprofiler_resync: (a: number) => void;
  readonly webprofiler_set_enabled: (a: number, b: number) => void;
  readonly webprofiler_flush: (a: number) => [number, number];
  readonly webprofiler_snapshot: (a: number, b: number) => [number, number];
  readonly webprofiler_live: (a: number) => number;
  readonly webprofiler_dropped: (a: number) => number;
  readonly webprofiler_produced: (a: number) => number;
  readonly webprofiler_capacity: (a: number) => number;
  readonly webprofiler_capturing: (a: number) => number;
  readonly page_pid: () => number;
  readonly tid_main: () => number;
  readonly tid_worker_base: () => number;
  readonly __wbindgen_exn_store: (a: number) => void;
  readonly __externref_table_alloc: () => number;
  readonly __wbindgen_export_2: WebAssembly.Table;
  readonly __wbindgen_malloc: (a: number, b: number) => number;
  readonly __wbindgen_realloc: (a: number, b: number, c: number, d: number) => number;
  readonly __wbindgen_free: (a: number, b: number, c: number) => void;
  readonly __wbindgen_start: () => void;
}

export type SyncInitInput = BufferSource | WebAssembly.Module;
/**
* Instantiates the given `module`, which can either be bytes or
* a precompiled `WebAssembly.Module`.
*
* @param {{ module: SyncInitInput }} module - Passing `SyncInitInput` directly is deprecated.
*
* @returns {InitOutput}
*/
export function initSync(module: { module: SyncInitInput } | SyncInitInput): InitOutput;

/**
* If `module_or_path` is {RequestInfo} or {URL}, makes a request and
* for everything else, calls `WebAssembly.instantiate` directly.
*
* @param {{ module_or_path: InitInput | Promise<InitInput> }} module_or_path - Passing `InitInput` directly is deprecated.
*
* @returns {Promise<InitOutput>}
*/
export default function __wbg_init (module_or_path?: { module_or_path: InitInput | Promise<InitInput> } | InitInput | Promise<InitInput>): Promise<InitOutput>;
