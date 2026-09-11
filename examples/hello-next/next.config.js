/** @type {import('next').NextConfig} */
// `standalone` makes `next build` emit a self-contained server under
// `.next/standalone/` with only the node_modules it actually traced — the
// small artifact ply packages. See this directory's README.
module.exports = {
  output: "standalone",
};
