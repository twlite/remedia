import { EventEmitter } from 'node:events';

type EventMap<Events> = { [Key in keyof Events]: unknown[] };
type EventKey<Key, Events> = Events extends [never] ? string | symbol : Key | keyof Events;
type EventArgs<Key, Events> = Events extends [never]
  ? any[]
  : Key extends keyof Events
    ? Events[Key]
    : never;

/**
 * EventEmitter-compatible base that isolates synchronous listener failures from engine work.
 * @typeParam Events - Type used to correlate typed event keys and argument tuples.
 */
export class RemediaEventEmitter<Events extends EventMap<Events>> extends EventEmitter<Events> {
  /**
   * Emit a typed lifecycle event while isolating exceptions thrown by application listeners.
   * @param event - Event name identifying the typed notification or Gateway voice dispatch.
   * @param args - Typed event arguments delivered to registered listeners.
   * @returns Whether at least one listener handled the event, or false when a listener threw.
   * @typeParam Key - Type used to correlate typed event keys and argument tuples.
   */
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
        // A custom process warning handler must not re-enter native or lifecycle operations.
      }
      return false;
    }
  }
}
