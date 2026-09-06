// The smallest useful server: one file, served as-is. PORT is set by ply
// when it hands the app a port (rootless runs), 8000 otherwise.
const http = require("http");
const fs = require("fs");

const port = Number(process.env.PORT) || 8000;
http
  .createServer((req, res) => {
    res.setHeader("Content-Type", "text/plain; charset=utf-8");
    res.end(fs.readFileSync("index.html"));
  })
  .listen(port, "0.0.0.0", () => console.log(`hello-http listening on ${port}`));
