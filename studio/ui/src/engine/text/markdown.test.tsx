import { describe, expect, it } from "vitest";
import { render } from "@testing-library/react";
import { renderMarkdown } from "./markdown";

const html = (text: string) => render(<div>{renderMarkdown(text)}</div>).container.innerHTML;

describe("renderMarkdown", () => {
  it("renders the little Markdown an agent writes as elements, never HTML", () => {
    const out = html("Two open tickets:\n\n- **#1** — Projector flickers (room 2A)\n- **#3** — Vending machine `jammed`\n\n1. first\n2. second\n\n## Done\nSee [the desk](https://desk.example.com) and [bad](javascript:alert(1)).\n\n```\nraw < & >\n```");
    expect(out).toContain("<ul class=\"rn-md__list\"><li><b>#1</b>");
    expect(out).toContain("<code class=\"rn-md__code\">jammed</code>");
    expect(out).toContain("<ol class=\"rn-md__list\"><li>first</li><li>second</li></ol>");
    expect(out).toContain("<p class=\"rn-md__h\"><b>Done</b></p>");
    expect(out).toContain("<a href=\"https://desk.example.com\" target=\"_blank\" rel=\"noreferrer noopener\">the desk</a>");
    expect(out).toContain("bad (javascript:alert(1)");
    expect(out).not.toContain("<a href=\"javascript");
    expect(out).toContain("<pre class=\"rn-md__pre\">raw &lt; &amp; &gt;</pre>");
  });

  it("keeps plain text on its own lines and leaves HTML as text", () => {
    const out = html("line one\nline two <script>x</script>");
    expect(out).toContain("line one</span><span><br>line two &lt;script&gt;x&lt;/script&gt;</span>");
  });
});
