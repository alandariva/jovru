#!/usr/bin/env node
// A `jjp/1` program for Jykre Studio's automation: it writes synthetic rows
// to a CSV file and returns how many it wrote and where. Node.js 18 or
// later, no dependencies. See README.md next to it.
//
// Inputs:  rows (number, optional, default 10), file (text, optional).
// Outputs: rows (number), file (text).
'use strict';

const fs = require('node:fs');
const net = require('node:net');
const os = require('node:os');
const path = require('node:path');

// The token is the first line of stdin, then end of file. Never print it.
function readToken() {
  return new Promise((resolve, reject) => {
    let text = '';
    process.stdin.setEncoding('utf8');
    process.stdin.on('data', (chunk) => {
      text += chunk;
    });
    process.stdin.on('error', reject);
    process.stdin.on('end', () => {
      try {
        const line = JSON.parse(text.split('\n')[0]);
        if (line.jjp !== '1' || typeof line.token !== 'string') {
          throw new Error('stdin does not start with a jjp/1 token line');
        }
        resolve(line.token);
      } catch (error) {
        reject(error);
      }
    });
  });
}

// One NDJSON JSON-RPC 2.0 connection to the host.
class Host {
  constructor(socket) {
    this.socket = socket;
    this.buffer = '';
    this.nextId = 1;
    this.pending = new Map();
    this.cancelled = false;
    // Decodes UTF-8 across chunk boundaries.
    socket.setEncoding('utf8');
    socket.on('data', (chunk) => this.receive(chunk));
    // 'close' always follows and rejects whatever is still waiting.
    socket.on('error', () => {});
    socket.on('close', () => {
      for (const waiting of this.pending.values()) {
        waiting.reject(new Error('the host closed the connection'));
      }
      this.pending.clear();
    });
  }

  receive(chunk) {
    this.buffer += chunk;
    let end;
    while ((end = this.buffer.indexOf('\n')) >= 0) {
      const line = this.buffer.slice(0, end);
      this.buffer = this.buffer.slice(end + 1);
      if (line.trim() !== '') {
        this.dispatch(JSON.parse(line));
      }
    }
  }

  dispatch(message) {
    // The only message the host sends on its own.
    if (message.method === 'cancel') {
      this.cancelled = true;
      return;
    }
    const waiting = this.pending.get(message.id);
    if (!waiting) {
      return;
    }
    this.pending.delete(message.id);
    if (message.error) {
      const error = new Error(message.error.message);
      error.code = message.error.code;
      error.data = message.error.data;
      waiting.reject(error);
    } else {
      waiting.resolve(message.result);
    }
  }

  send(message) {
    this.socket.write(JSON.stringify(message) + '\n');
  }

  request(method, params) {
    const id = this.nextId++;
    const answer = new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
    });
    this.send({ jsonrpc: '2.0', id, method, params });
    return answer;
  }

  notify(method, params) {
    this.send({ jsonrpc: '2.0', method, params });
  }

  log(level, message) {
    this.notify('log', { level, message });
  }
}

function connect(address) {
  const separator = address.lastIndexOf(':');
  const host = address.slice(0, separator);
  const port = Number(address.slice(separator + 1));
  return new Promise((resolve, reject) => {
    const socket = net.createConnection({ host, port }, () => resolve(socket));
    socket.once('error', reject);
  });
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function main() {
  const token = await readToken();
  const address = process.env.JYKRE_JJP_ADDRESS;
  if (!address) {
    throw new Error('JYKRE_JJP_ADDRESS is not set: run this from Jykre Studio');
  }
  const socket = await connect(address);
  const host = new Host(socket);
  const hello = await host.request('hello', { token, protocol: 'jjp/1' });

  // Inputs arrive as text, or null when the form left them empty.
  const rows = hello.inputs.rows == null ? 10 : Number(hello.inputs.rows);
  if (!Number.isInteger(rows) || rows < 0 || rows > 1000000) {
    await host.request('fail', {
      code: 'BAD_INPUT',
      message: 'rows must be a whole number from 0 to 1000000',
    });
    socket.end();
    return;
  }
  const file =
    hello.inputs.file || path.join(os.tmpdir(), `jjp-example-${hello.run_id}.csv`);

  host.log('info', `exporting ${rows} rows`);
  const lines = ['id,name'];
  const batch = Math.max(1, Math.ceil(rows / 20));
  for (let row = 1; row <= rows; row++) {
    if (host.cancelled) {
      // Stop asked us to end: say so and leave; Studio kills the program
      // if it is still running 5 s after asking.
      await host.request('fail', { code: 'CANCELLED', message: `stopped after ${row - 1} rows` });
      socket.end();
      return;
    }
    lines.push(`${row},row ${row}`);
    if (row % batch === 0 || row === rows) {
      host.notify('progress', {
        percent: Math.floor((row * 100) / rows),
        stage: 'writing rows',
      });
      // Simulated work; it also lets a `cancel` arrive.
      await sleep(20);
    }
  }
  fs.writeFileSync(file, lines.join('\n') + '\n');

  // Every declared output, as text (never a JSON number) or null.
  await host.request('complete', { outputs: { rows: String(rows), file } });
  socket.end();
}

main().catch((error) => {
  // stderr goes to the run's stderr.log; keep values and secrets out of it.
  // Exit once it is written, even with the connection still open.
  process.stderr.write(`export failed: ${error.message}\n`, () => process.exit(1));
});
