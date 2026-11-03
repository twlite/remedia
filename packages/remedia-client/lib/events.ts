import { EventEmitter } from 'node:events';

type EventMap<Events> = { [Key in keyof Events]: unknown[] };
type EventKey<Key, Events> = Events extends [never] ? string | symbol : Key | keyof Events;
type EventArgs<Key, Events> = Events extends [never]
  ? any[]
  : Key extends keyof Events
    ? Events[Key]
    : never;

/** EventEmitter-compatible base that prevents listeners from disrupting RPC event processing. */
export class RemediaEventEmitter<Events extends EventMap<Events>> extends EventEmitter<Events> {
  override emit<Key>(event: EventKey<Key, Events>, ...args: EventArgs<Key, Events>): boolean {
    try {
      return super.emit(event, ...args);
    } catch {
      try {
        process.emitWarning(
          `A listener for Remedia event ${String(event)} threw; the exception was isolated.`,
          { code: 'REMEDIA_LISTENER_ERROR' },
        );
      } catch {
        // A custom process warning handler must not re-enter RPC event processing.
      }
      return false;
    }
  }
}
