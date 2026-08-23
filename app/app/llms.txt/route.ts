import { llmsIndex } from "@/lib/ai-discovery";

export function GET() {
  return new Response(llmsIndex(), {
    headers: {
      "Content-Type": "text/markdown; charset=utf-8",
      "Cache-Control": "public, max-age=3600, s-maxage=86400",
    },
  });
}
