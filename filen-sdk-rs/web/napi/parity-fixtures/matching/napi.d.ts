// napi-shaped typings that differ from ./ubrn only in what the parity check normalizes:
// optional keys, readonly, symbol keys, tag literals for enum members, a class for an interface.

export interface Listener {
  onEvent(event: Event): void
}

export type ThingInterface = Thing

export declare class Thing {
  name(): string
  size(asyncOpts_?: { signal: AbortSignal }): Promise<bigint>
  static create(config: Config): Thing
}

export interface Config {
  label?: string | undefined
  limit: bigint
}

export type Event =
  | { tag: 'Started'; inner: { at: bigint } }
  | { tag: 'Stopped' }

export declare enum Event_Tags {
  Started = 'Started',
  Stopped = 'Stopped',
}

export declare enum Level {
  Off = 0,
  On = 1,
}

export declare function setLevel(level: Level): void
