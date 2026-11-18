// Types of the TripleTrail JavaScript client (#223).

/** A term of a SPARQL JSON result. */
export interface Term {
  type: "uri" | "literal" | "bnode" | "triple";
  value: string;
  datatype?: string;
  "xml:lang"?: string;
}

export type Row = Record<string, Term>;

export interface SelectResult {
  variables: string[];
  rows: Row[];
  /** The server cut the answer at its row budget (`partial: true`). */
  incomplete: boolean;
}

export type Value = string | number | bigint | boolean;

export function toValue(term: Term | undefined): Value | undefined;
export function toValues(result: SelectResult): Record<string, Value | undefined>[];

export class TripleTrailError extends Error {
  status: number;
  body: string;
}

export interface ClientOptions {
  /** The dataset of queries, updates and graphs (`ds`). */
  dataset?: string;
  /** Sent as `Authorization: Bearer`. */
  token?: string;
  /** Sent as `X-API-Key`. */
  apiKey?: string;
  headers?: Record<string, string>;
  fetch?: typeof fetch;
}

export interface QueryOptions {
  /** `default-graph-uri`: the default graph of the query. */
  defaultGraphs?: string[];
  /** `named-graph-uri`: its named graphs. */
  namedGraphs?: string[];
  /** Seconds; the server takes it if it is below its own timeout. */
  timeout?: number;
  /** A row budget below the server's. */
  maxRows?: number;
  /** Answer the first rows of a larger result instead of refusing it (413). */
  partial?: boolean;
  /** The format of the answer, for `query` and `construct`. */
  accept?: string;
  signal?: AbortSignal;
}

export interface UpdateOptions {
  /** `using-graph-uri`. */
  usingGraphs?: string[];
  /** `using-named-graph-uri`. */
  usingNamedGraphs?: string[];
  timeout?: number;
  signal?: AbortSignal;
}

export interface RequestOptions {
  params?: Record<string, string | number | string[] | undefined | null>;
  headers?: Record<string, string>;
  body?: BodyInit;
  signal?: AbortSignal;
}

/** A graph IRI, or `null` (or `"default"`) for the default graph. */
export type GraphName = string | null;

export class TripleTrail {
  constructor(url: string, options?: ClientOptions);
  readonly url: string;
  readonly datasetName: string;
  readonly graphs: GraphStore;
  readonly admin: Admin;
  dataset(name: string): TripleTrail;
  request(method: string, path: string, options?: RequestOptions): Promise<Response>;
  query(query: string, options?: QueryOptions): Promise<Response>;
  select(query: string, options?: QueryOptions): Promise<SelectResult>;
  ask(query: string, options?: QueryOptions): Promise<boolean>;
  construct(query: string, options?: QueryOptions): Promise<string>;
  update(update: string, options?: UpdateOptions): Promise<void>;
}

export class GraphStore {
  get(graph: GraphName, options?: { accept?: string; signal?: AbortSignal }): Promise<string>;
  put(graph: GraphName, data: BodyInit, options?: { contentType?: string; signal?: AbortSignal }): Promise<void>;
  post(graph: GraphName, data: BodyInit, options?: { contentType?: string; signal?: AbortSignal }): Promise<void>;
  delete(graph: GraphName, options?: { signal?: AbortSignal }): Promise<void>;
}

export interface CreateDatasetOptions {
  type?: "mem" | "disk";
  location?: string;
  mode?: string;
  aliasOf?: string;
  [parameter: string]: string | undefined;
}

export class Admin {
  ping(): Promise<boolean>;
  ready(): Promise<{ ready: boolean; [key: string]: unknown }>;
  server(): Promise<Record<string, unknown>>;
  datasets(): Promise<Record<string, unknown>[]>;
  dataset(name: string): Promise<Record<string, unknown>>;
  createDataset(name: string, options?: CreateDatasetOptions): Promise<void>;
  deleteDataset(name: string, options?: { deleteFiles?: boolean; force?: boolean }): Promise<void>;
  stats(name?: string): Promise<Record<string, unknown>>;
}
