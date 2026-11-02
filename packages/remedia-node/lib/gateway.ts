/** Minimal Discord Gateway integration used by the Node.js voice adapter. */
export interface DiscordGatewayVoicePayload {
  op: 4;
  d: {
    guild_id: string;
    channel_id: string | null;
    self_mute: false;
    self_deaf: false;
  };
}

/** Send a Gateway payload through the application's existing Discord client or shard. */
export type DiscordGatewaySend = (
  guildId: string,
  payload: DiscordGatewayVoicePayload,
) => void | Promise<void>;

/** A raw Discord Gateway packet, such as the value emitted by a client's `raw` event. */
export interface DiscordGatewayPacket {
  t?: string;
  d?: unknown;
  op?: number;
  [key: string]: unknown;
}

export type DiscordVoiceGatewayEvent = 'VOICE_STATE_UPDATE' | 'VOICE_SERVER_UPDATE';

export interface DiscordVoiceGatewayDispatch {
  event: DiscordVoiceGatewayEvent;
  data: Record<string, unknown>;
  guildId: string;
  sequence?: number;
}

/**
 * Adapts the caller-owned Discord Gateway to Remedia's voice lifecycle. The adapter only sends
 * Gateway OP 4 and parses the two voice dispatches; voice WebSocket, UDP, RTP, and DAVE remain in
 * the native transport.
 */
export class DiscordGatewayAdapter {
  readonly #send: DiscordGatewaySend;
  #lastSequence: number | undefined;

  constructor(send: DiscordGatewaySend) {
    this.#send = send;
  }

  /** Most recently observed Gateway dispatch sequence, when supplied by the host client. */
  get lastSequence(): number | undefined {
    return this.#lastSequence;
  }

  /** Request a voice channel join or leave through Gateway OP 4. */
  setVoiceChannel(guildId: string, channelId: string | null): void | Promise<void> {
    return this.#send(guildId, {
      op: 4,
      d: {
        guild_id: guildId,
        channel_id: channelId,
        self_mute: false,
        self_deaf: false,
      },
    });
  }

  /** Parse only relevant voice dispatches; malformed and unrelated packets are ignored. */
  receive(packet: unknown): DiscordVoiceGatewayDispatch | undefined {
    if (!isRecord(packet) || typeof packet.t !== 'string') return undefined;
    if (typeof packet.s === 'number' && Number.isSafeInteger(packet.s) && packet.s >= 0) {
      if (
        packet.t === 'READY' ||
        this.#lastSequence === undefined ||
        packet.s > this.#lastSequence
      ) {
        this.#lastSequence = packet.s;
      }
    }
    if (!isRecord(packet.d)) return undefined;
    if (packet.t !== 'VOICE_STATE_UPDATE' && packet.t !== 'VOICE_SERVER_UPDATE') return undefined;

    const guildId = packet.d.guild_id;
    if (typeof guildId !== 'string' || !isSnowflake(guildId)) return undefined;

    const dispatch: DiscordVoiceGatewayDispatch = {
      event: packet.t,
      data: packet.d,
      guildId,
    };
    if (typeof packet.s === 'number' && Number.isSafeInteger(packet.s) && packet.s >= 0) {
      dispatch.sequence = packet.s;
    }
    return dispatch;
  }
}

export function isSnowflake(value: string): boolean {
  if (!/^\d{1,20}$/.test(value)) return false;
  try {
    const id = BigInt(value);
    return id > 0n && id <= 18_446_744_073_709_551_615n;
  } catch {
    return false;
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}
