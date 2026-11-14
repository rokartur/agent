// The globals a Code script sees; `tool()` hands each call to the Rust loop in code.rs.
const __pending = new Map();
let __nextCall = 0;

globalThis.tool = (name, input) => {
  const call = new Promise((resolve, reject) => {
    const id = __nextCall++;
    __pending.set(id, { resolve, reject });
    __request(id, String(name), JSON.stringify(input ?? {}));
  });
  call.nothrow = () => call.catch((error) => error.message);
  return call;
};

globalThis.__settle = (id, ok, text) => {
  const call = __pending.get(id);
  __pending.delete(id);
  if (ok) call.resolve(text);
  else call.reject(new Error(text));
};

const __quote = (value) => (Array.isArray(value) ? value.map(__quote).join(" ") : `'${String(value).replaceAll("'", `'\\''`)}'`);
globalThis.$ = (strings, ...values) => tool("Bash", { command: String.raw(strings, ...values.map(__quote)) });

// QuickJS has no console, and a script that logs instead of returning must not die on a ReferenceError after its calls ran.
const __logs = [];
const __logged = (value) => (typeof value === "object" && !(value instanceof Error) ? JSON.stringify(value) : String(value));
globalThis.console = {};
for (const level of ["log", "info", "warn", "error", "debug"]) console[level] = (...values) => void __logs.push(values.map(__logged).join(" "));

// What reaches the model: the lines the script logged, then its value (a string as is, anything else as JSON) or its error.
globalThis.__done = (ok, value) => {
  let last = [];
  if (!ok) last = [value && value.stack ? `${value}\n${value.stack}` : String(value)];
  else if (typeof value === "string") last = [value];
  else if (value !== undefined) last = [JSON.stringify(value, null, 2) ?? String(value)];
  else if (__logs.length === 0) last = ["The script returned no value."];
  __finish(ok, [...__logs, ...last].join("\n"));
};
