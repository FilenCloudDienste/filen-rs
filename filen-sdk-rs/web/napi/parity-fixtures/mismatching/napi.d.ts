// napi-shaped typings with three real differences from ./ubrn: `Config.limit` is a number where
// ubrn has a bigint, `Listener` lacks ubrn's `onError`, and `Thing` has no `ThingInterface`.

export interface Config {
  label?: string | undefined
  limit: number
}

export interface Listener {
  onEvent(event: string): void
}

export declare class Thing {
  name(): string
}
