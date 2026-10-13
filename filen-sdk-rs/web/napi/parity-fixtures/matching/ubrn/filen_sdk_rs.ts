// ubrn-shaped counterpart of ../napi.d.ts.
// @ts-nocheck
import { UniffiAbstractObject, UniffiEnum, uniffiTypeNameSymbol } from "uniffi-bindgen-react-native"

export interface Listener {
	onEvent(event: Event): void
}

export interface ThingInterface {
	name(): string
	size(asyncOpts_?: { signal: AbortSignal }): Promise<bigint>
	other(): void
}

export class Thing extends UniffiAbstractObject implements ThingInterface {
	readonly [uniffiTypeNameSymbol] = "Thing"
	private constructor() {
		super()
	}
	name(): string {
		return ""
	}
	async size(asyncOpts_?: { signal: AbortSignal }): Promise<bigint> {
		return 0n
	}
	other(): void {}
	static create(config: Config): ThingInterface {
		return new Thing()
	}
	uniffiDestroy(): void {}
}

export type Config = {
	label: string | undefined
	limit: bigint
}

export enum Event_Tags {
	Started = "Started",
	Stopped = "Stopped"
}

export const Event = (() => {
	class Started_ extends UniffiEnum {
		readonly [uniffiTypeNameSymbol] = "Event"
		readonly tag = Event_Tags.Started
		readonly inner: Readonly<{ at: bigint }>
		constructor(inner: { at: bigint }) {
			super("Event", "Started")
			this.inner = Object.freeze(inner)
		}
	}
	class Stopped_ extends UniffiEnum {
		readonly [uniffiTypeNameSymbol] = "Event"
		readonly tag = Event_Tags.Stopped
		constructor() {
			super("Event", "Stopped")
		}
	}
	function instanceOf(obj: any): obj is Event {
		return obj[uniffiTypeNameSymbol] === "Event"
	}
	return Object.freeze({ instanceOf, Started: Started_, Stopped: Stopped_ })
})()

export type Event = InstanceType<(typeof Event)[keyof Omit<typeof Event, "instanceOf">]>

export enum Level {
	Off,
	On
}

export function setLevel(level: Level): void {}
