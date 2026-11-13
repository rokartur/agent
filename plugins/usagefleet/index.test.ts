import { describe, expect, test } from "bun:test";
import { accountLabels, assignToken, blockMessage, claudeAccount, formatGroupLimits, footer, formatLimits, hasFiveHourWindow, inheritToken, inputAnswer, resolveToken, tokenReport, type Context } from "./index.ts";

const now = Date.parse("2026-10-15T12:00:00Z");
const context: Context = { provider: "anthropic", model: "claude-opus-5-5", accountId: "uuid-2", accountName: "home", cwd: "/", sessionId: "s", settings: { showGroupName: false } };

describe("usagefleet", () => {
	test("shows fresh account limits and hides stale ones", () => {
		const limits = { fiveHourPct: 5.4, sevenDayPct: 41, at: "2026-10-15T11:52:00Z" };
		expect(formatLimits({ limits }, now)).toEqual([{ text: "5h " }, { text: "5%", tone: "ok" }, { text: " · 7d " }, { text: "41%", tone: "ok" }]);
		expect(formatLimits({ limits: { ...limits, at: "2026-10-15T11:40:00Z" } }, now)).toBeNull();
	});

	test("shows only the weekly window on a plan without a 5h window", () => {
		const ultra = { limits: { fiveHourPct: null, sevenDayPct: 41, at: "2026-10-15T11:52:00Z" } };
		expect(formatLimits(ultra, now)).toEqual([{ text: "7d " }, { text: "41%", tone: "ok" }]);
		expect(hasFiveHourWindow(context, ultra)).toBe(false);
		expect(hasFiveHourWindow(context, null)).toBe(true);
		// The watcher reads another account, so the agent's reading of the session's account decides.
		expect(hasFiveHourWindow({ ...context, accountUsage: { fiveHourPct: null, sevenDayPct: 41 } }, null)).toBe(false);
		expect(hasFiveHourWindow({ ...context, accountUsage: { fiveHourPct: 0, sevenDayPct: 41 } }, ultra)).toBe(true);
		const view = { group: "Artur", sessionPct: 0, weeklyPct: 17 };
		expect(formatGroupLimits(view, true, false)?.map((part) => part.text).join("")).toBe("Artur: 7d 17%");
	});

	test("falls back to the agent's reading of an account without a token or the watcher", () => {
		const text = (accountUsage: Context["accountUsage"]) => footer({ ...context, accountUsage }, null, null, now)?.map((part) => part.text).join("");
		expect(text({ fiveHourPct: 12, sevenDayPct: 40 })).toBe("5h 12% · 7d 40%");
		expect(text({ fiveHourPct: null, sevenDayPct: 40 })).toBe("7d 40%");
		expect(text(null)).toBeUndefined();
	});

	test("tones percents green below 50, orange below 80, red from 80", () => {
		const tones = (fiveHourPct: number, sevenDayPct: number) =>
			formatLimits({ limits: { fiveHourPct, sevenDayPct, at: "2026-10-15T11:58:00Z" } }, now)?.map((part) => part.tone);
		expect(tones(49.4, 49.5)).toEqual([undefined, "ok", undefined, "warn"]);
		expect(tones(79.4, 79.5)).toEqual([undefined, "warn", undefined, "error"]);
	});

	test("shows the group budget, named only when asked", () => {
		const view = { group: "Artur", sessionPct: 30, weeklyPct: 17 };
		expect(formatGroupLimits(view, true, true)?.map((part) => part.text).join("")).toBe("Artur: 5h 30% · 7d 17%");
		expect(formatGroupLimits(view, false, true)?.map((part) => part.text).join("")).toBe("5h 30% · 7d 17%");
		expect(formatGroupLimits({ group: null }, true, true)).toBeNull();
	});

	test("picks the session account's token, never another account's", () => {
		const config = { token: "shared", accountTokens: { "uuid-1": "first", "uuid-2": "second" } };
		expect(resolveToken(config, "uuid-2", {})).toBe("second");
		expect(resolveToken(config, "uuid-9", {})).toBe("shared");
		expect(resolveToken({ token: "first", accountTokens: { "uuid-1": "first" } }, "uuid-9", {})).toBeNull();
		expect(resolveToken({ accountTokens: { "uuid-1": "first" } }, "uuid-9", {})).toBeNull();
		expect(resolveToken(config, "uuid-2", { USAGEFLEET_TOKEN: "env" })).toBe("env");
	});

	test("assigns a token to one account and keeps the rest of the config", () => {
		const config = { token: "shared", state: { deviceId: "d" }, accountTokens: { "uuid-1": "first" } };
		expect(assignToken(config, "uuid-2", "uf_second")).toEqual({
			token: "shared",
			state: { deviceId: "d" },
			accountTokens: { "uuid-1": "first", "uuid-2": "uf_second" },
		});
	});

	test("a replaced account's token is copied to its new account id", () => {
		const config = { token: "shared", accountTokens: { "uuid-1": "first" } };
		expect(inheritToken(config, "uuid-1", "uuid-3")).toEqual({ token: "shared", accountTokens: { "uuid-1": "first", "uuid-3": "first" } });
		expect(inheritToken(config, "uuid-2", "uuid-3")).toBeNull();
	});

	test("labels each account with its assigned token, masked", () => {
		const config = { token: "uf_sharedtoken", accountTokens: { "uuid-1": "uf_firsttoken", "uuid-2": "" } };
		expect(accountLabels(config)).toEqual({ "uuid-1": "uf_firs…" });
		expect(accountLabels(null)).toEqual({});
	});

	test("reports which token the account uses without showing it", () => {
		const config = { token: "uf_sharedtoken", accountTokens: { "uuid-1": "uf_firsttoken" } };
		expect(tokenReport(config, "uuid-1", "work", {})).toBe("Account work uses its own token uf_firs….");
		expect(tokenReport(config, "uuid-2", "home", {})).toStartWith("Account home has no token of its own and uses uf_shar….");
		expect(tokenReport({ token: "uf_firsttoken", accountTokens: { "uuid-1": "uf_firsttoken" } }, "uuid-2", "home", {})).toStartWith("Account home has no usagefleet token.");
		expect(tokenReport({}, "uuid-2", "home", {})).toStartWith("Account home has no usagefleet token.");
	});

	test("guards only Claude subscription accounts", () => {
		expect(claudeAccount(context)).toBe("uuid-2");
		expect(claudeAccount({ ...context, accountId: null })).toBeNull();
		expect(claudeAccount({ ...context, provider: "openai-codex" })).toBeNull();
	});

	test("refuses only when the server says blocked", () => {
		expect(blockMessage({ blocked: true, group: "Artur", blockedWindow: "weekly", blockedPct: 100 }, "claude-opus-5-5")).toBe(
			'usagefleet: "Artur" has used 100% of its weekly budget, so new prompts are blocked.',
		);
		expect(blockMessage({ blocked: false, group: "Artur" }, "claude-opus-5-5")).toBeNull();
	});

	test("a block names the reset, so the agent moves to its next account", () => {
		const blocked = { blocked: true, group: "artur", blockedWindow: "weekly", blockedPct: 101.3, blockedUntil: "2026-11-16T11:59:00.000Z" };
		expect(inputAnswer(blocked, "claude-opus-5-5")).toMatchObject({ action: "block", accountBlockedUntil: "2026-11-16T11:59:00.000Z" });
		expect(inputAnswer({ blocked: false }, "claude-opus-5-5")).toEqual({ action: "continue" });
	});

	test("a per-model limit blocks only that model", () => {
		const fable = { blocked: true, group: "artur", blockedWindow: "weekly", blockedModel: "Fable", blockedPct: 152 };
		expect(blockMessage(fable, "claude-opus-5-5")).toBeNull();
		expect(blockMessage(fable, "claude-fable-1")).toStartWith('usagefleet: "artur" has used 152% of its Fable weekly budget');
	});

	test("words the reset as dd/mm/yy in local time", () => {
		const until = new Date(2026, 10, 16, 13, 0);
		expect(blockMessage({ blocked: true, blockedUntil: until.toISOString() }, "claude-opus-5-5")).toEndWith(" Resets 16/11/26, 13:00.");
	});
});
