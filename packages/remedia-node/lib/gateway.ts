/** Minimal Discord Gateway integration used by the Node.js voice adapter. */
export interface DiscordGatewayVoicePayload {
  /** Discord Gateway operation code; voice state requests use 4. */
  op: 4;
  /** Gateway payload data; voice requests contain guild, channel, and self-state fields. */
  d: {
    /** Discord guild snowflake to route through the bot Gateway. */
    guild_id: string;
    /** Voice channel snowflake, or null to leave voice. */
    channel_id: string | null;
    /** Discord self-mute flag; independent of playback volume and pause. */
    self_mute: boolean;
    /** Discord self-deaf flag; independent of playback volume and pause. */
    self_deaf: boolean;
  };
}

/**
 * Send a Gateway payload through the application's existing Discord client or shard.
 * @param guildId - Discord guild snowflake used to route the operation.
 * @param payload - Discord Gateway OP 4 payload to send through the guild shard.
 */
export type DiscordGatewaySend = (
  guildId: string,
  payload: DiscordGatewayVoicePayload,
) => void | Promise<void>;

/** A raw Discord Gateway packet, such as the value emitted by a client's `raw` event. */
export interface DiscordGatewayPacket {
  /** Gateway dispatch event name, when the packet represents a dispatch. */
  t?: string;
  /** Raw event-specific Gateway payload, validated only for supported voice dispatches. */
  d?: unknown;
  /** Gateway operation code, when supplied by the host application. */
  op?: number;
  /**
   * Additional Gateway packet fields forwarded unchanged by the host application.
   * @param key - Additional Gateway packet field name.
   */
  [key: string]: unknown;
}

/** Gateway dispatch names used to obtain voice transport credentials. */
export type DiscordVoiceGatewayEvent = 'VOICE_STATE_UPDATE' | 'VOICE_SERVER_UPDATE';

/** Validated guild-scoped voice dispatch with sequence and Gateway epoch correlation. */
export interface DiscordVoiceGatewayDispatch {
  /** Gateway voice dispatch name. */
  event: DiscordVoiceGatewayEvent;
  /** Event-specific structured payload. */
  data: Record<string, unknown>;
  /** Discord guild snowflake as a decimal string; unique within the owning client. */
  guildId: string;
  /** Host Gateway dispatch sequence used to reject stale voice credentials. */
  sequence?: number;
  /** Gateway READY epoch used to distinguish independent dispatch sequence spaces. */
  epoch: number;
}

/**
 * Adapts the caller-owned Discord Gateway to Remedia's voice lifecycle. The adapter only sends
 * Gateway OP 4 and parses the two voice dispatches; voice WebSocket, UDP, RTP, and DAVE remain in
 * the native transport.
 */
export class DiscordGatewayAdapter {
  readonly #send: DiscordGatewaySend;
  #lastSequence: number | undefined;
  #epoch = 0;

  /**
   * Construct this wrapper with its owning services. Use the owning engine or client factory for application code.
   * @param send - Application callback routing Gateway OP 4 through the correct guild shard.
   */
  constructor(send: DiscordGatewaySend) {
    this.#send = send;
  }

  /** Most recently observed Gateway dispatch sequence, when supplied by the host client. */
  get lastSequence(): number | undefined {
    return this.#lastSequence;
  }

  /** Changes when the host Gateway starts a new READY session and resets its sequence. */
  get epoch(): number {
    return this.#epoch;
  }

  /**
   * Request a voice channel join or leave through Gateway OP 4.
   * @param guildId - Discord guild snowflake used to route the operation.
   * @param channelId - Destination Discord voice channel snowflake; null requests a leave where supported.
   * @param selfDeaf - Discord self-deaf flag; defaults to true.
   * @param selfMute - Discord self-mute flag; defaults to false.
   */
  setVoiceChannel(
    guildId: string,
    channelId: string | null,
    selfDeaf = true,
    selfMute = false,
  ): void | Promise<void> {
    return this.#send(guildId, {
      op: 4,
      d: {
        guild_id: guildId,
        channel_id: channelId,
        self_mute: selfMute,
        self_deaf: selfDeaf,
      },
    });
  }

  /**
   * Parse only relevant voice dispatches; malformed and unrelated packets are ignored.
   * @param packet - Raw packet from this bot's Gateway dispatch stream.
   * @returns Validated voice dispatch, or undefined for an unrelated or malformed packet.
   */
  receive(packet: unknown): DiscordVoiceGatewayDispatch | undefined {
    if (!isRecord(packet) || typeof packet.t !== 'string') return undefined;
    const sequence =
      typeof packet.s === 'number' && Number.isSafeInteger(packet.s) && packet.s >= 0
        ? packet.s
        : undefined;
    if (packet.t === 'READY') {
      this.#epoch += 1;
      this.#lastSequence = sequence;
      return undefined;
    }
    if (sequence !== undefined) {
      if (this.#lastSequence !== undefined && sequence <= this.#lastSequence) return undefined;
      this.#lastSequence = sequence;
    }
    if (!isRecord(packet.d)) return undefined;
    if (packet.t !== 'VOICE_STATE_UPDATE' && packet.t !== 'VOICE_SERVER_UPDATE') return undefined;

    const guildId = packet.d.guild_id;
    if (typeof guildId !== 'string' || !isSnowflake(guildId)) return undefined;

    const dispatch: DiscordVoiceGatewayDispatch = {
      event: packet.t,
      data: packet.d,
      guildId,
      epoch: this.#epoch,
    };
    if (sequence !== undefined) dispatch.sequence = sequence;
    return dispatch;
  }
}

/**
 * Check whether a string contains a positive decimal Discord snowflake that fits u64.
 * @param value - Discord snowflake string to validate.
 * @returns True for a valid positive u64 snowflake string.
 */
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
