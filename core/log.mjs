export function format(level, message, fields = {}) {
  const rest = Object.entries(fields)
    .filter(([, value]) => value !== undefined)
    .map(([key, value]) => `${key}=${JSON.stringify(value)}`)
    .join(" ");
  return rest ? `${level} ${message} ${rest}` : `${level} ${message}`;
}

export function createLog(write = (line) => process.stderr.write(`${line}\n`)) {
  const emit = (level) => (message, fields) => write(format(level, message, fields));
  return { info: emit("info"), warn: emit("warn"), error: emit("error") };
}

export function createMemoryLog() {
  const lines = [];
  return { ...createLog((line) => lines.push(line)), lines };
}
