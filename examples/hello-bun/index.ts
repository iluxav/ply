// The smallest useful Bun server: one file, served as-is. PORT is set by ply
// when it hands the app a port (rootless runs), 3000 otherwise.
const port = Number(process.env.PORT) || 3000;

Bun.serve({
  port,
  hostname: "0.0.0.0",
  fetch() {
    return new Response(Bun.file("index.html"), {
      headers: { "Content-Type": "text/plain; charset=utf-8" },
    });
  },
});

console.log(`hello-bun listening on ${port}`);
