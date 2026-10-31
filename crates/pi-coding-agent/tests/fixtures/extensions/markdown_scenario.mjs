// pi-tui Markdown cases of the upstream changes after the old vendored snapshot: #11159, marked-cjk-friendly,
// 8a2ebd3, cd1ea342a, and fa1685152. Run by tests/extension_markdown.rs and scripts/markdown-scenario.mjs.
const plain = (text) => text;
const theme = {
	heading: plain,
	link: plain,
	linkUrl: plain,
	code: plain,
	codeBlock: plain,
	codeBlockBorder: plain,
	quote: plain,
	quoteBorder: plain,
	hr: plain,
	listBullet: plain,
	bold: (text) => `\x1b[1m${text}\x1b[22m`,
	italic: (text) => `\x1b[3m${text}\x1b[23m`,
	strikethrough: plain,
	underline: plain,
};

const table = ["| A | B |", "|---|---|", "| 1 | 2 |"];
const wideMath = [
	"\\[",
	String.raw`\frac{\partial^2 u}{\partial x^2} + \frac{\partial^2 u}{\partial y^2} = \sum_{i=1}^{n} \frac{1}{i^2} + \int_0^1 f(x)\,dx`,
	"\\]",
].join("\n");

export function scenario({ Markdown, setCapabilities, resetCapabilitiesCache }) {
	const render = (source, width, hyperlinks = false, paddingX = 0) => {
		setCapabilities({ images: null, trueColor: false, hyperlinks });
		try {
			return new Markdown(source, paddingX, 0, theme).render(width).map((line) => line.trimEnd());
		} finally {
			resetCapabilitiesCache();
		}
	};
	const rendered = {
		mathBeforeTable: render(["Intro", "", "$$", "", ...table, "", "After"].join("\n"), 80),
		bracketBeforeTable: render(["\\[", "x = 1", "", ...table].join("\n"), 80),
		cjkBold: render("**这是测试。**后面", 80),
		cjkQuoteBold: render("字**「引用」**字", 80),
		cjkItalic: render("*斜体。*后面", 80),
		latinFlanking: render("**end.**Then next", 80),
		deepQuote: render(`${">".repeat(60)}x`, 80, false, 1),
		wideMath40: render(wideMath, 40),
		wideMath30: render(wideMath, 30),
		urlComma: render("https://example.com，后面的中文", 160, true),
		urlPathPeriod: render("https://example.com/中文?q=搜索#片段。后面的中文", 160, true),
		urlWwwBracket: render("www.example.com」后面的中文", 160, true),
		urlCjkLetters: render("https://example.com紧接着中文", 160, true),
		explicitLink: render("[文档](https://example.com/中文，路径·テスト・한글…—。)", 160, true),
		angleLink: render("<https://example.com/中文，路径·テスト・한글…—。>", 160, true),
		latinTrimFirst: render("https://example.com.，后面的中文", 160, true),
	};
	return { rendered };
}
