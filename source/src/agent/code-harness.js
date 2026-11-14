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
