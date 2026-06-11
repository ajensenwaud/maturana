// Reconnecting WebSocket client with typed dispatch.
// Protocol v1: internally-tagged JSON messages (see ws/protocol.rs).

const PROTOCOL_VERSION = 1;

export class CockpitSocket {
  constructor() {
    this.handlers = new Map(); // type -> Set<fn>
    this.statusHandlers = new Set();
    this.queue = [];
    this.socket = null;
    this.backoff = 500;
    this.connect();
  }

  connect() {
    const scheme = window.location.protocol === "https:" ? "wss" : "ws";
    this.socket = new WebSocket(`${scheme}://${window.location.host}/ws`);

    this.socket.addEventListener("open", () => {
      this.backoff = 500;
      this.emitStatus("connecting");
    });

    this.socket.addEventListener("message", (event) => {
      let message;
      try {
        message = JSON.parse(event.data);
      } catch {
        return;
      }
      if (message.type === "hello") {
        if (message.v !== PROTOCOL_VERSION) {
          this.emitStatus("version-mismatch");
          this.socket.close();
          return;
        }
        this.emitStatus("open");
        for (const queued of this.queue.splice(0)) {
          this.socket.send(queued);
        }
      }
      const handlers = this.handlers.get(message.type);
      if (handlers) {
        for (const handler of handlers) handler(message);
      }
    });

    this.socket.addEventListener("close", () => {
      this.emitStatus("closed");
      setTimeout(() => this.connect(), this.backoff);
      this.backoff = Math.min(this.backoff * 2, 15000);
    });

    this.socket.addEventListener("error", () => {
      this.socket.close();
    });
  }

  on(type, handler) {
    if (!this.handlers.has(type)) this.handlers.set(type, new Set());
    this.handlers.get(type).add(handler);
    return () => this.handlers.get(type)?.delete(handler);
  }

  onStatus(handler) {
    this.statusHandlers.add(handler);
  }

  emitStatus(status) {
    for (const handler of this.statusHandlers) handler(status);
  }

  send(message) {
    const text = JSON.stringify(message);
    if (this.socket?.readyState === WebSocket.OPEN) {
      this.socket.send(text);
    } else {
      this.queue.push(text);
    }
  }

  subscribe(topics) {
    this.send({ type: "subscribe", topics });
  }
}
