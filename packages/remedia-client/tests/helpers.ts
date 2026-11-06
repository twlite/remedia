import { createServer } from 'node:http';
import { WebSocket, WebSocketServer } from 'ws';

interface FakePlayback {
  state: string;
  generation: number;
  processingMode: string | null;
  position: {
    media: { positionMs: number; accuracy: string } | null;
    submittedMs: number;
    playedMs: number | null;
  };
  durationMs: number | null;
  source: Record<string, unknown> | null;
  filterGraph: string | null;
  volume: number;
  lastError: { code: string; message: string } | null;
}

interface FakeGuild {
  guildSessionId: string;
  guildId: string;
  botUserId: string;
  playback: FakePlayback;
  voiceState: string | null;
  voiceGeneration: number;
}

interface FakeSession {
  id: string;
  credential: string;
  botUserId: string;
  guilds: Map<string, FakeGuild>;
  sequence: number;
  attached: WebSocket | undefined;
}

export interface FakeServerOptions {
  id: string;
  botUserId?: string;
  protocolVersion?: number;
  playersMaximum?: number;
  dropSessionsOnDisconnect?: boolean;
  requiredAuthorization?: string;
}

/** A local protocol-v1 node fixture used by client integration tests. */
export class FakeServerNode {
  readonly id: string;
  readonly botUserId: string;
  readonly protocolVersion: number;
  readonly playersMaximum: number;
  readonly authorizationHeaders: string[] = [];
  readonly botUserIdHeaders: string[] = [];
  readonly sessions = new Map<string, FakeSession>();
  readonly server = createServer();
  readonly #websocketServer: WebSocketServer;
  readonly #socketSessions = new Map<WebSocket, FakeSession>();
  readonly #socketBotUserIds = new Map<WebSocket, string>();
  readonly #dropSessionsOnDisconnect: boolean;
  readonly #requiredAuthorization: string | undefined;
  #sessionSequence = 0;
  #guildSequence = 0;
  #closed = false;
  finishBeforePlayResponse = false;
  playerPlayRequests = 0;
  probeGate: (() => Promise<void>) | undefined;
  probeCancelRequests = 0;
  voiceConnectRequests = 0;
  readonly voiceConnectionCredentials: Array<{
    sessionId: string;
    token: string;
    endpoint: string;
  }> = [];
  voiceDisconnectRequests = 0;
  voiceSuspendRequests = 0;
  voiceConnectFailures = 0;
  voiceConnectGate: (() => Promise<void>) | undefined;
  nodeIdentity: string;
  url = '';

  get allocatedGuildCount(): number {
    return [...this.sessions.values()].reduce((total, session) => total + session.guilds.size, 0);
  }

  private constructor(options: FakeServerOptions, websocketServer: WebSocketServer) {
    this.id = options.id;
    this.nodeIdentity = `server-${options.id}`;
    this.botUserId = options.botUserId ?? '100000000000000001';
    this.protocolVersion = options.protocolVersion ?? 1;
    this.playersMaximum = options.playersMaximum ?? 8;
    this.#dropSessionsOnDisconnect = options.dropSessionsOnDisconnect ?? false;
    this.#requiredAuthorization = options.requiredAuthorization;
    this.#websocketServer = websocketServer;
    websocketServer.on('connection', (socket) => {
      socket.on('message', (message) => {
        void this.#handle(socket, JSON.parse(message.toString()) as Record<string, unknown>);
      });
      socket.on('close', () => {
        const session = this.#socketSessions.get(socket);
        if (session?.attached === socket) session.attached = undefined;
        this.#socketSessions.delete(socket);
        this.#socketBotUserIds.delete(socket);
        if (session && this.#dropSessionsOnDisconnect) this.sessions.delete(session.id);
      });
    });
  }

  static async listen(options: FakeServerOptions): Promise<FakeServerNode> {
    const websocketServer = new WebSocketServer({ noServer: true });
    const node = new FakeServerNode(options, websocketServer);
    node.server.on('upgrade', (request, socket, head) => {
      node.authorizationHeaders.push(request.headers.authorization ?? '');
      if (
        node.#requiredAuthorization !== undefined &&
        request.headers.authorization !== node.#requiredAuthorization
      ) {
        socket.write('HTTP/1.1 401 Unauthorized\r\nConnection: close\r\n\r\n');
        socket.destroy();
        return;
      }
      const botUserId = request.headers['x-remedia-bot-id'];
      node.botUserIdHeaders.push(typeof botUserId === 'string' ? botUserId : '');
      if (!isSnowflake(botUserId)) {
        socket.write('HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n');
        socket.destroy();
        return;
      }
      websocketServer.handleUpgrade(request, socket, head, (websocket) => {
        node.#socketBotUserIds.set(websocket, botUserId);
        websocketServer.emit('connection', websocket, request);
      });
    });
    await new Promise<void>((resolve) => node.server.listen(0, '127.0.0.1', resolve));
    const address = node.server.address();
    if (!address || typeof address === 'string')
      throw new Error('fake server did not bind a TCP port');
    node.url = `ws://127.0.0.1:${address.port}/v1/ws`;
    return node;
  }

  closeSockets(): void {
    for (const socket of this.#socketSessions.keys()) socket.close(1011, 'test socket reset');
  }

  setPlaybackState(guildId: string, state: FakePlayback['state']): void {
    const guild = this.#findGuild(guildId);
    if (guild) guild.playback.state = state;
  }

  sendEvent(guildId: string, name: string, data: Record<string, unknown> = {}): void {
    const session = [...this.sessions.values()].find((item) => item.guilds.has(guildId));
    if (session) this.#event(session, name, { guildId, ...data });
  }

  async close(): Promise<void> {
    if (this.#closed) return;
    this.#closed = true;
    this.closeSockets();
    await new Promise<void>((resolve) => this.#websocketServer.close(() => resolve()));
    await new Promise<void>((resolve) => this.server.close(() => resolve()));
  }

  #event(session: FakeSession, name: string, data: Record<string, unknown>): void {
    const socket = session.attached;
    if (!socket || socket.readyState !== WebSocket.OPEN) return;
    session.sequence += 1;
    socket.send(
      JSON.stringify({
        version: 1,
        event: name,
        sessionId: session.id,
        sequence: session.sequence,
        data,
      }),
    );
  }

  async #handle(socket: WebSocket, message: Record<string, unknown>): Promise<void> {
    const id = typeof message.id === 'string' ? message.id : null;
    const command = typeof message.command === 'string' ? message.command : '';
    const params = isRecord(message.params) ? message.params : {};
    let result: unknown = {};
    let error: { code: string; message: string } | null = null;
    try {
      switch (command) {
        case 'media.probe':
          await this.probeGate?.();
          result = {
            streamIndex: 0,
            codec: 'mp3',
            sampleRate: 44100,
            channels: 2,
            durationMs: null,
            bitRate: null,
            metadata: { title: 'Probe fixture' },
          };
          break;
        case 'media.probe.cancel':
          this.probeCancelRequests++;
          result = { cancelled: true };
          break;
        case 'node.protocol_version':
          result = { version: this.protocolVersion };
          break;
        case 'node.info':
          result = {
            nodeId: this.nodeIdentity,
            version: 'test',
            protocolVersion: this.protocolVersion,
          };
          break;
        case 'node.health':
          result = { status: 'ok' };
          break;
        case 'node.stats':
          result = {
            nodeId: this.nodeIdentity,
            version: 'test',
            protocolVersion: 1,
            uptimeMs: 123,
            connections: { active: this.sessions.size, maximum: 64 },
            players: {
              active: [...this.sessions.values()].reduce(
                (count, session) => count + session.guilds.size,
                0,
              ),
              maximum: this.playersMaximum,
            },
            cpu: { availableParallelism: 2, loadAverage1m: null },
            memory: { residentBytes: null },
            transportHealth: 'healthy',
            processingHealth: 'healthy',
          };
          break;
        case 'client.create': {
          const session: FakeSession = {
            id: `session-${this.id}-${++this.#sessionSequence}`,
            credential: `resume-${this.id}-${this.#sessionSequence}`,
            botUserId: this.#socketBotUserIds.get(socket) ?? this.botUserId,
            guilds: new Map(),
            sequence: 0,
            attached: socket,
          };
          this.sessions.set(session.id, session);
          this.#socketSessions.set(socket, session);
          result = {
            sessionId: session.id,
            resumeCredential: session.credential,
            botUserId: session.botUserId,
          };
          break;
        }
        case 'client.resume': {
          const session = this.sessions.get(String(params.sessionId));
          if (
            !session ||
            session.credential !== params.resumeCredential ||
            session.attached ||
            session.botUserId !== this.#socketBotUserIds.get(socket)
          ) {
            error = { code: 'unknown_session', message: 'session unavailable' };
            break;
          }
          session.attached = socket;
          this.#socketSessions.set(socket, session);
          result = {
            sessionId: session.id,
            resumeCredential: session.credential,
            snapshot: { botUserId: session.botUserId, guilds: [...session.guilds.values()] },
          };
          break;
        }
        case 'client.close': {
          const session = this.#socketSessions.get(socket);
          if (session) this.sessions.delete(session.id);
          result = { closed: true };
          break;
        }
        case 'client.list': {
          const botUserId = this.#socketBotUserIds.get(socket);
          result = {
            sessions: [...this.sessions.values()]
              .filter((session) => session.botUserId === botUserId)
              .map((session) => ({
                sessionId: session.id,
                attached: Boolean(session.attached),
              })),
          };
          break;
        }
        case 'guild.create': {
          const session = this.#session(socket);
          const guildId = String(params.guildId);
          if (session.guilds.has(guildId)) {
            error = { code: 'invalid_state', message: 'guild exists' };
            break;
          }
          const guild = this.#newGuild(guildId, session.botUserId);
          session.guilds.set(guildId, guild);
          this.#event(session, 'guild.created', guild as unknown as Record<string, unknown>);
          result = guild;
          break;
        }
        case 'guild.list':
          result = { guilds: [...this.#session(socket).guilds.values()] };
          break;
        case 'guild.get':
          result = this.#guild(socket, String(params.guildId));
          break;
        case 'guild.destroy': {
          const session = this.#session(socket);
          session.guilds.delete(String(params.guildId));
          this.#event(session, 'guild.destroyed', { guildId: params.guildId as string });
          result = { destroyed: true };
          break;
        }
        case 'voice.connect': {
          const session = this.#session(socket);
          const guild = this.#guild(socket, String(params.guildId));
          const generation = Number(params.transportGeneration ?? guild.voiceGeneration + 1);
          if (generation < guild.voiceGeneration) {
            error = { code: 'invalid_state', message: 'voice attempt was superseded' };
            break;
          }
          if (generation === guild.voiceGeneration && guild.voiceState === 'connected') {
            result = { guildId: guild.guildId, state: 'connected', alreadyConnected: true };
            break;
          }
          guild.voiceGeneration = generation;
          this.voiceConnectRequests += 1;
          this.voiceConnectionCredentials.push({
            sessionId: String(params.sessionId),
            token: String(params.token),
            endpoint: String(params.endpoint),
          });
          const gate = this.voiceConnectGate;
          if (gate) await gate();
          if (this.voiceConnectFailures > 0) {
            this.voiceConnectFailures -= 1;
            error = { code: 'transport_failed', message: 'voice transport setup failed' };
            break;
          }
          if (guild.voiceGeneration !== generation) {
            error = { code: 'invalid_state', message: 'voice attempt was superseded' };
            break;
          }
          guild.voiceState = 'connected';
          this.#event(session, 'voice.state_changed', {
            guildId: guild.guildId,
            state: 'connected',
          });
          result = { guildId: guild.guildId, state: 'connected' };
          break;
        }
        case 'voice.suspend': {
          const session = this.#session(socket);
          const guild = this.#guild(socket, String(params.guildId));
          const generation = Number(params.transportGeneration ?? guild.voiceGeneration + 1);
          if (generation < guild.voiceGeneration) {
            error = { code: 'invalid_state', message: 'voice attempt was superseded' };
            break;
          }
          guild.voiceGeneration = generation;
          this.voiceSuspendRequests += 1;
          guild.voiceState = 'reconnecting';
          this.#event(session, 'voice.state_changed', {
            guildId: guild.guildId,
            state: 'reconnecting',
          });
          result = { guildId: guild.guildId, state: 'reconnecting' };
          break;
        }
        case 'voice.disconnect': {
          const session = this.#session(socket);
          const guild = this.#guild(socket, String(params.guildId));
          const generation = Number(params.transportGeneration ?? guild.voiceGeneration + 1);
          if (generation < guild.voiceGeneration) {
            error = { code: 'invalid_state', message: 'voice attempt was superseded' };
            break;
          }
          guild.voiceGeneration = generation;
          this.voiceDisconnectRequests += 1;
          guild.voiceState = 'disconnected';
          guild.playback.state = 'stopped';
          this.#event(session, 'voice.state_changed', {
            guildId: guild.guildId,
            state: 'disconnected',
          });
          result = { guildId: guild.guildId, state: 'disconnected' };
          break;
        }
        case 'voice.fail': {
          const session = this.#session(socket);
          const guild = this.#guild(socket, String(params.guildId));
          const generation = Number(params.transportGeneration ?? guild.voiceGeneration + 1);
          if (generation < guild.voiceGeneration) {
            error = { code: 'invalid_state', message: 'voice attempt was superseded' };
            break;
          }
          guild.voiceGeneration = generation;
          guild.voiceState = 'failed';
          guild.playback.state = 'failed';
          this.#event(session, 'voice.failed', { guildId: guild.guildId, state: 'failed' });
          result = { guildId: guild.guildId, state: 'failed' };
          break;
        }
        case 'voice.state': {
          const guild = this.#guild(socket, String(params.guildId));
          result = { state: guild.voiceState ?? 'disconnected', failed: false };
          break;
        }
        case 'player.state':
          result = this.#guild(socket, String(params.guildId)).playback;
          break;
        case 'player.play': {
          this.playerPlayRequests += 1;
          const session = this.#session(socket);
          const guild = this.#guild(socket, String(params.guildId));
          const generation = ++guild.playback.generation;
          guild.playback.state = 'playing';
          guild.playback.source = {
            streamIndex: 0,
            codec: 'mp3',
            sampleRate: 44_100,
            channels: 2,
            channelLayout: 'stereo',
            durationMs: 3_000,
            bitRate: null,
            metadata: { title: 'Fixture track' },
          };
          guild.playback.durationMs = 3_000;
          this.#event(session, 'player.started', {
            guildId: guild.guildId,
            generation,
            source: guild.playback.source,
          });
          if (this.finishBeforePlayResponse) {
            guild.playback.state = 'completed';
            this.#event(session, 'player.completed', { guildId: guild.guildId, generation });
          }
          result = { ...guild.playback.source, generation };
          break;
        }
        case 'player.pause':
        case 'player.resume':
        case 'player.stop': {
          const session = this.#session(socket);
          const guild = this.#guild(socket, String(params.guildId));
          const state =
            command === 'player.pause'
              ? 'paused'
              : command === 'player.resume'
                ? 'playing'
                : 'stopped';
          guild.playback.state = state;
          const eventName =
            command === 'player.pause'
              ? 'player.paused'
              : command === 'player.resume'
                ? 'player.resumed'
                : 'player.stopped';
          this.#event(session, eventName, {
            guildId: guild.guildId,
            generation: guild.playback.generation,
            previousState:
              command === 'player.pause'
                ? 'playing'
                : command === 'player.resume'
                  ? 'paused'
                  : 'playing',
            state,
            ...(command === 'player.stop' ? { reason: 'stop' } : {}),
          });
          result = { state };
          break;
        }
        case 'player.seek': {
          const session = this.#session(socket);
          const guild = this.#guild(socket, String(params.guildId));
          const previousPositionMs = guild.playback.position.media?.positionMs ?? null;
          guild.playback.position.media = {
            positionMs: Number(params.positionMs),
            accuracy: 'timestamp',
          };
          this.#event(session, 'player.seeked', {
            guildId: guild.guildId,
            generation: guild.playback.generation,
            positionMs: Number(params.positionMs),
            previousPositionMs,
          });
          result = { ...guild.playback.position };
          break;
        }
        case 'player.filter.set': {
          const session = this.#session(socket);
          const guild = this.#guild(socket, String(params.guildId));
          guild.playback.filterGraph = String(params.graph);
          this.#event(session, 'player.filter_changed', {
            guildId: guild.guildId,
            generation: guild.playback.generation,
            previousGraph: null,
            graph: guild.playback.filterGraph,
          });
          result = { filterGraph: guild.playback.filterGraph };
          break;
        }
        case 'player.filter.clear': {
          const session = this.#session(socket);
          const guild = this.#guild(socket, String(params.guildId));
          const previousGraph = guild.playback.filterGraph;
          guild.playback.filterGraph = null;
          this.#event(session, 'player.filter_changed', {
            guildId: guild.guildId,
            generation: guild.playback.generation,
            previousGraph,
            graph: null,
          });
          result = { filterGraph: null };
          break;
        }
        case 'player.volume.set': {
          const session = this.#session(socket);
          const guild = this.#guild(socket, String(params.guildId));
          const previousVolume = guild.playback.volume;
          guild.playback.volume = Number(params.gain);
          this.#event(session, 'player.volume_changed', {
            guildId: guild.guildId,
            generation: guild.playback.generation,
            previousVolume,
            volume: guild.playback.volume,
            gain: guild.playback.volume,
          });
          result = { volume: guild.playback.volume };
          break;
        }
        case 'test.delay': {
          await delay(Number(params.delayMs));
          result = { value: params.value };
          break;
        }
        default:
          error = { code: 'invalid_request', message: 'unknown command' };
      }
    } catch {
      error = { code: 'unknown_session', message: 'session or player is missing' };
    }

    const response = error
      ? { version: 1, id, ok: false, error }
      : { version: 1, id, ok: true, result };
    socket.send(JSON.stringify(response));
  }

  #session(socket: WebSocket): FakeSession {
    const session = this.#socketSessions.get(socket);
    if (!session || session.attached !== socket) throw new Error('no client session');
    return session;
  }

  #guild(socket: WebSocket, guildId: string): FakeGuild {
    const guild = this.#session(socket).guilds.get(guildId);
    if (!guild) throw new Error('no guild session');
    return guild;
  }

  #findGuild(guildId: string): FakeGuild | undefined {
    for (const session of this.sessions.values()) {
      const guild = session.guilds.get(guildId);
      if (guild) return guild;
    }
    return undefined;
  }

  #newGuild(guildId: string, botUserId: string): FakeGuild {
    return {
      guildSessionId: `guild-session-${++this.#guildSequence}`,
      guildId,
      botUserId,
      voiceState: 'disconnected',
      voiceGeneration: 0,
      playback: {
        state: 'idle',
        generation: 0,
        processingMode: 'transcoding',
        position: { media: null, submittedMs: 0, playedMs: null },
        durationMs: null,
        source: null,
        filterGraph: null,
        volume: 1,
        lastError: null,
      },
    };
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function isSnowflake(value: string | string[] | undefined): value is string {
  if (typeof value !== 'string' || !/^\d{1,20}$/.test(value)) return false;
  try {
    return BigInt(value) > 0n && BigInt(value) <= 18_446_744_073_709_551_615n;
  } catch {
    return false;
  }
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
