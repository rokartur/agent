#!/usr/bin/env bun
/**
 * Shows this device's group budget in the footer, or its usagefleet limits (5h + weekly) outside a group,
 * and refuses prompts while that budget is blocked.
 *
 * usagefleet's watcher writes the device's last limits report into its config file every minute.
 * The group budget lives only on the server, so it is fetched with the device token of the
 * session's Claude account, the same request `usagefleet guard` makes per prompt.
 */
import { mkdir, readFile, rename, writeFile } from "node:fs/promises";
import { dirname } from "node:path";
import { homedir } from "node:os";
import { join } from "node:path";

/** What agent sends with every event and input request. */
export type Context = {
	provider: string;
	model: string;
	/** The Claude account uuid; null for API keys. */
	accountId: string | null;
	/** The name the user gave the account at /login. */
	accountName: string | null;
	cwd: string;
	sessionId: string;
	settings: { showGroupName: boolean };
};

const SERVER_LIMITS_URL = "https://usagefleet.com/api/v1/limits";
const CONFIG_PATH = process.env.USAGEFLEET_CONFIG ?? join(homedir(), ".config", "usagefleet", "config.json");
const COLLECTOR_PATHS = [
	join(homedir(), ".config", "usagefleet", "pi-claude", ".claude.json"),
	process.env.CLAUDE_CONFIG_DIR ? join(process.env.CLAUDE_CONFIG_DIR, ".claude.json") : join(homedir(), ".claude.json"),
];
/** The watcher rewrites an unchanged reading's `at` only every 10 minutes, so this must outlast that. */
const STALE_AFTER_MS = 15 * 60_000;
/** The watcher reports every minute and nothing in the agent fires while it idles, so poll. */
const REFRESH_MS = 30_000;

const isRecord = (value: unknown): value is Record<string, unknown> => typeof value === "object" && value !== null;

/** A `ui.setStatus` part; the footer colors it by tone. */
export type StatusPart = { text: string; tone?: "ok" | "warn" | "error" };

/** "5h 12% · 7d 8%" with toned percents, "7d 8%" on a plan without a 5h window, or null when the config has no fresh reading. */
export function formatLimits(config: unknown, now: number): StatusPart[] | null {
	const limits = isRecord(config) ? config.limits : undefined;
	if (!isRecord(limits)) return null;
	const { fiveHourPct, sevenDayPct, at } = limits;
	if ((fiveHourPct !== null && typeof fiveHourPct !== "number") || typeof sevenDayPct !== "number") return null;
	const reported = typeof at === "string" ? Date.parse(at) : Number.NaN;
	if (Number.isNaN(reported) || now - reported > STALE_AFTER_MS) return null;
	return formatWindows(fiveHourPct, sevenDayPct);
}

/** "Artur: 5h 30% · 7d 17%", or null when the device is in no group. */
export function formatGroupLimits(view: unknown, showName: boolean, hasFiveHour: boolean): StatusPart[] | null {
	if (!isRecord(view)) return null;
	const { group, sessionPct, weeklyPct } = view;
	if (typeof group !== "string" || typeof sessionPct !== "number" || typeof weeklyPct !== "number") return null;
	// The server sends sessionPct 0 for an Ultra plan, so only the watcher's reading tells it has no 5h window.
	const windows = formatWindows(hasFiveHour ? sessionPct : null, weeklyPct);
	return showName ? [{ text: `${group}: ` }, ...windows] : windows;
}

/** Ultra plans have no 5h window, so the watcher reports `fiveHourPct: null`. */
export const hasFiveHourWindow = (config: unknown): boolean =>
	!(isRecord(config) && isRecord(config.limits) && config.limits.fiveHourPct === null);

const formatWindows = (fiveHourPct: number | null, sevenDayPct: number): StatusPart[] =>
	fiveHourPct === null
		? [{ text: "7d " }, percentPart(sevenDayPct)]
		: [{ text: "5h " }, percentPart(fiveHourPct), { text: " · 7d " }, percentPart(sevenDayPct)];

function percentPart(pct: number): StatusPart {
	const rounded = Math.round(pct);
	const text = `${rounded}%`;
	if (rounded < 50) return { text, tone: "ok" };
	if (rounded < 80) return { text, tone: "warn" };
	return { text, tone: "error" };
}

export function detectAccountUuid(raw: unknown): string | null {
	if (!isRecord(raw)) return null;
	const account = raw.oauthAccount;
	if (!isRecord(account) || typeof account.accountUuid !== "string" || !account.accountUuid) return null;
	return account.accountUuid;
}

/** USAGEFLEET_TOKEN, then the account's device token, then the shared one unless it is another account's.
 * A token reports the subscription its device signs into, so another account's would show that account's budget. */
export function resolveToken(config: unknown, accountId: string, env: NodeJS.ProcessEnv = process.env): string | null {
	if (env.USAGEFLEET_TOKEN) return env.USAGEFLEET_TOKEN;
	if (!isRecord(config)) return null;
	const accountTokens = isRecord(config.accountTokens) ? config.accountTokens : {};
	const own = accountTokens[accountId];
	if (typeof own === "string" && own) return own;
	if (typeof config.token !== "string" || !config.token || Object.values(accountTokens).includes(config.token)) return null;
	return config.token;
}

/** usagefleet guards the Claude subscription; other providers and API keys have no budget here. */
export const claudeAccount = (context: Context): string | null =>
	context.provider === "anthropic" ? context.accountId : null;

/** "uf_tVdC…": enough to tell tokens apart without showing one. */
const maskToken = (token: string) => `${token.slice(0, 7)}…`;

/** What `/usagefleet` with no args says about the token the account's prompts use. */
export function tokenReport(config: unknown, accountId: string, accountName: string, env: NodeJS.ProcessEnv = process.env): string {
	const account = `Account ${accountName}`;
	if (env.USAGEFLEET_TOKEN) return `USAGEFLEET_TOKEN (${maskToken(env.USAGEFLEET_TOKEN)}) overrides the config for every account.`;
	const accountTokens = isRecord(config) && isRecord(config.accountTokens) ? config.accountTokens : {};
	const own = accountTokens[accountId];
	if (typeof own === "string" && own) return `${account} uses its own token ${maskToken(own)}.`;
	const fallback = resolveToken(config, accountId, env);
	const set = "Set its own with /usagefleet <token> from the usagefleet Devices page.";
	if (fallback) return `${account} has no token of its own and uses ${maskToken(fallback)}. ${set}`;
	return `${account} has no usagefleet token. ${set}`;
}

/** `/account` panel text per Claude account uuid: the token assigned to it, masked. */
export function accountLabels(config: unknown): Record<string, string> {
	const accountTokens = isRecord(config) && isRecord(config.accountTokens) ? config.accountTokens : {};
	const labels: Record<string, string> = {};
	for (const [accountId, token] of Object.entries(accountTokens)) {
		if (typeof token === "string" && token) labels[accountId] = maskToken(token);
	}
	return labels;
}

/** The config with `token` assigned to the account; every other key stays as it was. */
export function assignToken(config: Record<string, unknown>, accountId: string, token: string): Record<string, unknown> {
	const accountTokens = isRecord(config.accountTokens) ? config.accountTokens : {};
	return { ...config, accountTokens: { ...accountTokens, [accountId]: token } };
}

/** After /login overwrote a saved account with another Claude account: the config with the old account's token
 * copied to the new one, or null when the old account had no token of its own. */
export function inheritToken(config: Record<string, unknown>, previousId: string, accountId: string): Record<string, unknown> | null {
	const accountTokens = isRecord(config.accountTokens) ? config.accountTokens : {};
	const token = accountTokens[previousId];
	if (typeof token !== "string" || !token) return null;
	return assignToken(config, accountId, token);
}

/** Unlike readJson, a broken config throws: writing over it would lose the watcher's state. */
async function readConfigForWrite(): Promise<Record<string, unknown>> {
	const text = await readFile(CONFIG_PATH, "utf8").catch((error: NodeJS.ErrnoException) => {
		if (error.code === "ENOENT") return "{}";
		throw error;
	});
	const config: unknown = JSON.parse(text);
	if (!isRecord(config)) throw new Error(`${CONFIG_PATH} does not hold a JSON object.`);
	return config;
}

async function writeConfig(config: Record<string, unknown>): Promise<void> {
	await mkdir(dirname(CONFIG_PATH), { recursive: true });
	// The watcher reads this file every few seconds, so it must never see a half-written one.
	const partial = `${CONFIG_PATH}.${process.pid}.tmp`;
	await writeFile(partial, `${JSON.stringify(config, null, 2)}\n`, { mode: 0o600 });
	await rename(partial, CONFIG_PATH);
}

/** `/usagefleet [token]`: shows or sets the device token of the session's Claude account. */
async function usagefleetCommand(args: string, context: Context): Promise<string> {
	const account = claudeAccount(context);
	if (context.provider !== "anthropic") throw new Error("Switch to a Claude model first.");
	// Accounts saved before the agent stored account.uuid have none until the next /login.
	if (!account) throw new Error("This Claude account has no account id. API keys have none. For a subscription, run /login again.");
	const name = context.accountName ?? account;
	if (!args) return tokenReport(await readJson(CONFIG_PATH), account, name);
	if (/\s/.test(args)) throw new Error("Usage: /usagefleet <token>");
	await writeConfig(assignToken(await readConfigForWrite(), account, args));
	return `Account ${name} now uses token ${maskToken(args)}.`;
}
/** Mirrors `usagefleet guard`: the reason to refuse a prompt, or null unless the server says `blocked: true`. */
export function blockMessage(view: unknown): string | null {
	if (!isRecord(view) || view.blocked !== true) return null;
	const { group, blockedWindow, blockedModel, blockedPct, blockedUntil } = view;
	const window = blockedWindow === "weekly" ? "weekly" : "5h";
	const limit = typeof blockedModel === "string" && blockedModel ? `${blockedModel} ${window}` : window;
	const pct = typeof blockedPct === "number" ? Number(blockedPct.toFixed(1)) : 100;
	const who = typeof group === "string" && group ? `"${group}"` : "this group";
	const until = typeof blockedUntil === "string" ? new Date(blockedUntil) : null;
	const resets =
		until && !Number.isNaN(until.getTime())
			? ` Resets ${until.toLocaleString([], { dateStyle: "short", timeStyle: "short" })}.`
			: "";
	return `usagefleet: ${who} has used ${pct}% of its ${limit} budget, so new prompts are blocked.${resets}`;
}

const readJson = (path: string): Promise<unknown> =>
	readFile(path, "utf8")
		.then(JSON.parse)
		.catch(() => null);

async function collectorAccount(): Promise<string | null> {
	for (const path of COLLECTOR_PATHS) {
		const uuid = detectAccountUuid(await readJson(path));
		if (uuid) return uuid;
	}
	return null;
}

async function fetchServerLimits(token: string): Promise<unknown> {
	const response = await fetch(SERVER_LIMITS_URL, { headers: { "x-api-key": token }, signal: AbortSignal.timeout(4_000) });
	return response.ok ? response.json() : null;
}

async function status(context: Context): Promise<StatusPart[] | null> {
	const account = claudeAccount(context);
	if (!account) return null;
	const config = await readJson(CONFIG_PATH);
	const token = resolveToken(config, account);
	// Offline or rejected hides the group numbers rather than freezing old ones.
	const view = token ? await fetchServerLimits(token).catch(() => null) : null;
	// The group budget is what blocks prompts, so it replaces the account's own numbers.
	// The watcher reads limits for one account; a session on another account would show the wrong numbers.
	const ownReading = account === (await collectorAccount()) ? config : null;
	const group = formatGroupLimits(view, context.settings.showGroupName, hasFiveHourWindow(ownReading));
	if (group) return group;
	return ownReading ? formatLimits(ownReading, Date.now()) : null;
}

/** Fails open on every error, the same guarantee `usagefleet guard` makes. */
async function blockedReason(context: Context): Promise<string | null> {
	const account = claudeAccount(context);
	if (!account) return null;
	const token = resolveToken(await readJson(CONFIG_PATH), account);
	if (!token) return null;
	return blockMessage(await fetchServerLimits(token).catch(() => null));
}

const send = (message: object) => process.stdout.write(`${JSON.stringify({ jsonrpc: "2.0", ...message })}\n`);

type Message = {
	id?: number;
	method?: string;
	params?: { type?: string; args?: string; previousAccountId?: string; accountId?: string; context?: Context };
};

async function handle(message: Message): Promise<void> {
	const context = message.params?.context;
	if (!context) return;
	const type = message.params?.type;
	if (message.method === "event" && (type === "session_start" || type === "prompt_end" || type === "account_change")) {
		if (type === "session_start") await refreshAccountLabels();
		await refreshStatus(context);
	} else if (message.method === "event" && type === "account_replace") {
		const { previousAccountId, accountId } = message.params ?? {};
		if (!previousAccountId || !accountId) return;
		const config = inheritToken(await readConfigForWrite(), previousAccountId, accountId);
		if (!config) return;
		await writeConfig(config);
		await refreshAccountLabels();
		await refreshStatus(context);
	} else if (message.method === "input") {
		const reason = await blockedReason(context).catch(() => null);
		send({ id: message.id, result: reason ? { action: "block", reason } : { action: "continue" } });
	} else if (message.method === "command") {
		try {
			send({ id: message.id, result: { text: await usagefleetCommand(message.params?.args ?? "", context) } });
		} catch (error) {
			send({ id: message.id, error: { code: -32000, message: error instanceof Error ? error.message : String(error) } });
			return;
		}
		await refreshAccountLabels();
		await refreshStatus(context);
	}
}

async function refreshStatus(context: Context): Promise<void> {
	send({ method: "ui.setStatus", params: { text: await status(context).catch(() => null) } });
}

async function refreshAccountLabels(): Promise<void> {
	send({ method: "ui.setAccountLabels", params: { labels: accountLabels(await readJson(CONFIG_PATH)) } });
}

if (import.meta.main) {
	let latestContext: Context | undefined;
	const refresh = setInterval(() => {
		if (latestContext) refreshStatus(latestContext).catch(() => {});
	}, REFRESH_MS);
	for await (const line of console) {
		// Bun yields an empty line when stdin closes.
		if (!line) continue;
		const message: Message = JSON.parse(line);
		latestContext = message.params?.context ?? latestContext;
		// Not awaited: a slow status refresh must not hold up the next input answer.
		handle(message).catch(() => {});
	}
	clearInterval(refresh);
}
