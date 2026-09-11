export default function Home() {
  return (
    <main style={{ fontFamily: "system-ui, sans-serif", maxWidth: 640, margin: "10vh auto", padding: 24 }}>
      <h1>Hello from Next.js on ply</h1>
      <p>
        This page is served by the Next.js standalone server, packaged into a
        single deterministic ply image — no daemon, no Dockerfile.
      </p>
      <p>
        Rendered at <code>{new Date().toISOString()}</code>.
      </p>
    </main>
  );
}
