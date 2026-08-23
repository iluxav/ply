import { llmsFull } from "@/lib/ai-discovery";

export function GET() {
  return new Response(llmsFull(), {
    headers: {
      "Content-Type": "text/markdown; charset=utf-8",
      "Cache-Control": "public, max-age=3600, s-maxage=86400",
    },
  });
}
