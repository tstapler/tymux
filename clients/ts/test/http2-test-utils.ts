import type * as http2 from "node:http2";

// Node's http2.Server.close() only stops accepting NEW connections -- its
// callback fires only once every currently-open Http2Session has ended on
// its own. It never force-closes active sessions. A connect-node client
// keeps its session open for reuse, so if a test never explicitly closes
// the client side, server.close() waits forever for a session that has no
// reason to end within the test's lifetime.
//
// Root-caused via real CI diagnostic output (not guessed): a prior
// diagnostic pass (process.stdout.write logging around .listen(), since
// removed) proved .listen() itself always succeeds -- the hang was always
// in .close(), on CI specifically, where nothing ever prompted the
// client's session to close on its own before the test's teardown blocked
// on it.
//
// Fix: track every session the server accepts and destroy any still-open
// ones before/during close, so close()'s callback can actually fire.
export function trackHttp2Sessions(server: http2.Http2Server): Set<http2.Http2Session> {
  const sessions = new Set<http2.Http2Session>();
  server.on("session", (session) => {
    sessions.add(session);
    session.once("close", () => sessions.delete(session));
  });
  return sessions;
}

export function closeHttp2Server(server: http2.Http2Server, sessions: Set<http2.Http2Session>): Promise<void> {
  return new Promise((resolve) => {
    server.close(() => resolve());
    for (const session of sessions) {
      session.destroy();
    }
  });
}
