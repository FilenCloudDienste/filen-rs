// ubrn-shaped counterpart of ../napi.d.ts.
// @ts-nocheck

export type Config = {
	label: string | undefined
	limit: bigint
}

export interface Listener {
	onEvent(event: string): void
	onError(message: string): void
}

export interface ThingInterface {
	name(): string
}

export class Thing implements ThingInterface {
	name(): string {
		return ""
	}
}
