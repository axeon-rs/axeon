// Express 5, one worker per CPU core (Node's cluster module), so it isn't
// limited to a single core while the Rust servers use all of them.
const cluster = require("node:cluster");
const os = require("node:os");

if (cluster.isPrimary) {
  for (let i = 0; i < os.availableParallelism(); i++) cluster.fork();
} else {
  const express = require("express");
  const app = express();
  app.disable("x-powered-by");
  app.get("/plaintext", (req, res) => res.type("text").send("Hello, World!"));
  app.get("/json", (req, res) => res.json({ message: "Hello, World!" }));
  app.get("/users/:id", (req, res) => res.json({ id: Number(req.params.id), name: "user" }));
  app.listen(8004, "127.0.0.1");
}
