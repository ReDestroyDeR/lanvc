// lanvc room hub for Cloudflare Workers.
// Every client connects with a WebSocket to wss://<this-worker>/ and every
// binary frame is relayed to all other connected clients, uninterpreted.
// Packets are end-to-end encrypted with the room key — the hub is blind.
//
// Deploy (from the cloudflare/ directory):
//   npx wrangler deploy          # workers.dev URL, or
//   add a custom domain route (room.example.com) in the dashboard / wrangler.toml
//
// Clients then set Server = wss://room.example.com/ + the same room key.

export class Room {
  constructor(_state, _env) {}

  async fetch(request) {
    if (request.headers.get("Upgrade") !== "websocket") {
      return new Response("lanvc room — connect with a WebSocket", { status: 200 });
    }
    const pair = new WebSocketPair();
    this.state.acceptWebSocket(pair[1]);
    return new Response(null, { status: 101, webSocket: pair[0] });
  }

  // hibernatable handler: runs only when a frame arrives
  webSocketMessage(ws, message) {
    for (const other of this.state.getWebSockets()) {
      if (other !== ws) {
        try { other.send(message); } catch { /* drop on dead socket */ }
      }
    }
  }
}

export default {
  async fetch(request, env) {
    const id = env.ROOM.idFromName("room");
    return env.ROOM.get(id).fetch(request);
  },
};
