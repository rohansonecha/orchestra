/**
 * claude-look — render pi's built-in tools the way Claude Code does.
 *
 *   ● Bash(cargo test)
 *     ⎿  test result: ok. 118 passed
 *        … +12 lines (ctrl+o to expand)
 *   ● Read(src/main.rs)
 *     ⎿  Read 240 lines
 *
 * Tool behavior is unchanged: each tool is re-registered under its own
 * name and delegates execution to pi's built-in definition. Only the call
 * header and the collapsed result change: the dot is gray while running,
 * green when done and red on error; only the tool name is bold; output is
 * shown in the muted color, three lines at most until expanded. edit and
 * write keep pi's own rendering, which shows the diff.
 *
 * Installed by orchestra into ~/.pi/agent/extensions/.
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import {
	createBashToolDefinition,
	createFindToolDefinition,
	createGrepToolDefinition,
	createLsToolDefinition,
	createReadToolDefinition,
} from "@earendil-works/pi-coding-agent";
import { Text, truncateToWidth } from "@earendil-works/pi-tui";

const PREVIEW_LINES = 3;

type Ctx = { isPartial?: boolean; isError?: boolean; executionStarted?: boolean; cwd?: string };

function dot(theme: any, ctx: Ctx): string {
	if (ctx.isPartial || !ctx.executionStarted) return theme.fg("muted", "●");
	return ctx.isError ? theme.fg("error", "●") : theme.fg("success", "●");
}

function header(theme: any, ctx: Ctx, name: string, arg: string): Text {
	const shown = arg.length > 160 ? `${arg.slice(0, 157)}…` : arg;
	return new Text(`${dot(theme, ctx)} ${theme.bold(name)}(${shown})`, 0, 0);
}

/** Relative to cwd when inside it, like Claude Code shows paths. */
function rel(path: string | undefined, cwd: string | undefined): string {
	if (!path) return "";
	if (cwd && path.startsWith(`${cwd}/`)) return path.slice(cwd.length + 1);
	const home = process.env.HOME;
	if (home && path.startsWith(`${home}/`)) return `~/${path.slice(home.length + 1)}`;
	return path;
}

function textOf(result: any): string {
	return (result?.content ?? [])
		.filter((c: any) => c?.type === "text")
		.map((c: any) => c.text as string)
		.join("\n")
		.replace(/\s+$/, "");
}

/** A component drawn at the current width. */
function lines(render: (width: number) => string[]) {
	return { render, invalidate() {} };
}

/**
 * "  ⎿  first lines" block. Collapsed: three rows, each cut to one screen
 * line (long lines don't wrap into a wall of text), then a count of the
 * rest. Expanded: everything.
 */
function outputBlock(theme: any, ctx: Ctx, output: string, expanded: boolean, summary?: string) {
	const color = ctx.isError ? "error" : "muted";
	const gutter = (i: number) => (i === 0 ? `  ${theme.fg("muted", "⎿")}  ` : "     ");
	if (summary && !expanded) {
		return new Text(`${gutter(0)}${theme.fg(color, summary)}`, 0, 0);
	}
	const all = output.length ? output.split("\n") : [];
	if (!all.length) {
		return new Text(`${gutter(0)}${theme.fg("muted", ctx.isError ? "(failed, no output)" : "(no output)")}`, 0, 0);
	}
	if (expanded) {
		return new Text(all.map((l, i) => gutter(i) + theme.fg(color, l)).join("\n"), 0, 0);
	}
	return lines((width) => {
		const shown = all.slice(0, PREVIEW_LINES).map((l, i) => truncateToWidth(gutter(i) + theme.fg(color, l), width, "…"));
		const rest = all.length - PREVIEW_LINES;
		if (rest > 0) shown.push(`     ${theme.fg("muted", `… +${rest} lines (ctrl+o to expand)`)}`);
		return shown;
	});
}

export default function (pi: ExtensionAPI) {
	const cwd = process.cwd();

	const wrap = (def: any, name: string, arg: (a: any, ctx: Ctx) => string, summary?: (out: string, a: any) => string) => {
		pi.registerTool({
			...def,
			renderShell: "self",
			renderCall(args: any, theme: any, context: any) {
				return header(theme, context ?? {}, name, arg(args ?? {}, context ?? {}));
			},
			renderResult(result: any, options: any, theme: any, context: any) {
				const ctx: Ctx = context ?? {};
				if (options?.isPartial) {
					return new Text(`  ${theme.fg("muted", "⎿")}  ${theme.fg("muted", "Running…")}`, 0, 0);
				}
				const out = textOf(result);
				const s = summary && !ctx.isError ? summary(out, context?.args ?? {}) : undefined;
				return outputBlock(theme, ctx, out, !!options?.expanded, s);
			},
		});
	};

	wrap(createBashToolDefinition(cwd), "Bash", (a) => String(a.command ?? "…"));
	wrap(
		createReadToolDefinition(cwd),
		"Read",
		(a, ctx) => {
			const p = rel(a.path, ctx.cwd ?? cwd);
			if (a.offset || a.limit) {
				const from = a.offset ?? 1;
				return `${p}:${from}${a.limit ? `-${from + a.limit - 1}` : ""}`;
			}
			return p;
		},
		(out) => `Read ${out ? out.split("\n").length : 0} lines`,
	);
	wrap(
		createGrepToolDefinition(cwd),
		"Search",
		(a, ctx) => `${a.pattern ?? ""}${a.path ? ` in ${rel(a.path, ctx.cwd ?? cwd)}` : ""}`,
		(out) => `Found ${out ? out.split("\n").length : 0} lines`,
	);
	wrap(
		createFindToolDefinition(cwd),
		"Find",
		(a, ctx) => `${a.pattern ?? ""}${a.path ? ` in ${rel(a.path, ctx.cwd ?? cwd)}` : ""}`,
		(out) => `Found ${out ? out.split("\n").length : 0} files`,
	);
	wrap(
		createLsToolDefinition(cwd),
		"List",
		(a, ctx) => rel(a.path ?? ".", ctx.cwd ?? cwd),
		(out) => `Listed ${out ? out.split("\n").length : 0} entries`,
	);
}
