type JsonLdObject = Record<string, unknown>;

export function JsonLd({ data }: { data: JsonLdObject }) {
  const json = JSON.stringify(data).replace(/</g, "\\u003c");

  return (
    <script
      type="application/ld+json"
      dangerouslySetInnerHTML={{ __html: json }}
    />
  );
}
