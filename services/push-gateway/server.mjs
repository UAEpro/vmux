#!/usr/bin/env node

import { createHash, randomBytes, timingSafeEqual } from 'node:crypto';
import { mkdirSync, readFileSync, renameSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:http';
import { homedir } from 'node:os';
import { dirname, join } from 'node:path';
import { pathToFileURL } from 'node:url';

const EXPO_PUSH_URL = 'https://exp.host/--/api/v2/push/send';
const MAX_BODY_BYTES = 16 * 1024;
const CHALLENGE_TTL_MS = 2 * 60 * 1000;
const START_WINDOW_MS = 10 * 60 * 1000;
const START_LIMIT = 5;
const SEND_WINDOW_MS = 60 * 60 * 1000;
const SEND_LIMIT = 60;
const MIN_SEND_GAP_MS = 3_000;
const EVENT_TTL_MS = 24 * 60 * 60 * 1000;

const PUSH_TOKEN_RE = /^(?:ExponentPushToken|ExpoPushToken)\[[A-Za-z0-9_-]{10,}\]$/;
const HEX_32_RE = /^[a-f0-9]{64}$/;
const REGISTRATION_ID_RE = /^[a-f0-9]{32,64}$/;

const sha256 = (value) => createHash('sha256').update(value).digest('hex');

function constantTimeHexEqual(left, right) {
  if (!HEX_32_RE.test(left) || !HEX_32_RE.test(right)) return false;
  return timingSafeEqual(Buffer.from(left, 'hex'), Buffer.from(right, 'hex'));
}

function safeString(value, maxLength = 128) {
  if (typeof value !== 'string') return '';
  return value.replace(/[\u0000-\u001f\u007f]/g, '').slice(0, maxLength);
}

function readState(path) {
  try {
    const parsed = JSON.parse(readFileSync(path, 'utf8'));
    if (parsed?.version !== 1 || !Array.isArray(parsed.registrations)) {
      throw new Error('unsupported state format');
    }
    const registrations = new Map();
    for (const item of parsed.registrations) {
      if (HEX_32_RE.test(item?.token_hash) && HEX_32_RE.test(item?.secret_hash)) {
        registrations.set(item.token_hash, {
          tokenHash: item.token_hash,
          secretHash: item.secret_hash,
          createdAt: Number(item.created_at) || Date.now(),
        });
      }
    }
    return registrations;
  } catch (error) {
    if (error?.code === 'ENOENT') return new Map();
    throw new Error(`refusing to start with invalid gateway state: ${error.message}`);
  }
}

function persistState(path, registrations) {
  mkdirSync(dirname(path), { recursive: true, mode: 0o700 });
  const payload = JSON.stringify(
    {
      version: 1,
      registrations: [...registrations.values()]
        .sort((a, b) => a.tokenHash.localeCompare(b.tokenHash))
        .map((item) => ({
          token_hash: item.tokenHash,
          secret_hash: item.secretHash,
          created_at: item.createdAt,
        })),
    },
    null,
    2,
  );
  const temporary = `${path}.tmp-${process.pid}-${randomBytes(4).toString('hex')}`;
  writeFileSync(temporary, `${payload}\n`, { mode: 0o600 });
  renameSync(temporary, path);
}

async function readJson(request) {
  const chunks = [];
  let length = 0;
  for await (const chunk of request) {
    length += chunk.length;
    if (length > MAX_BODY_BYTES) {
      const error = new Error('request body too large');
      error.status = 413;
      throw error;
    }
    chunks.push(chunk);
  }
  if (length === 0) return {};
  try {
    return JSON.parse(Buffer.concat(chunks).toString('utf8'));
  } catch {
    const error = new Error('invalid JSON');
    error.status = 400;
    throw error;
  }
}

function response(reply, status, body) {
  const payload = body === undefined ? '' : JSON.stringify(body);
  reply.writeHead(status, {
    'Cache-Control': 'no-store',
    'Content-Length': Buffer.byteLength(payload),
    'Content-Type': 'application/json; charset=utf-8',
    'Cross-Origin-Resource-Policy': 'same-site',
    'Referrer-Policy': 'no-referrer',
    'X-Content-Type-Options': 'nosniff',
  });
  reply.end(payload);
}

function bearer(request) {
  const authorization = request.headers.authorization ?? '';
  return authorization.startsWith('Bearer ') ? authorization.slice(7).trim() : '';
}

function clientAddress(request) {
  const cloudflare = request.headers['cf-connecting-ip'];
  return safeString(Array.isArray(cloudflare) ? cloudflare[0] : cloudflare, 64)
    || request.socket.remoteAddress
    || 'unknown';
}

function allowWithinWindow(bucket, key, now, windowMs, limit) {
  const current = (bucket.get(key) ?? []).filter((time) => now - time < windowMs);
  if (current.length >= limit) {
    bucket.set(key, current);
    return false;
  }
  current.push(now);
  bucket.set(key, current);
  return true;
}

async function defaultExpoSender(accessToken, message) {
  const result = await fetch(EXPO_PUSH_URL, {
    method: 'POST',
    headers: {
      Accept: 'application/json',
      Authorization: `Bearer ${accessToken}`,
      'Content-Type': 'application/json',
    },
    body: JSON.stringify(message),
    signal: AbortSignal.timeout(10_000),
  });
  if (!result.ok) throw new Error(`Expo returned HTTP ${result.status}`);
  const payload = await result.json();
  const ticket = Array.isArray(payload?.data) ? payload.data[0] : payload?.data;
  if (!ticket || ticket.status !== 'ok') {
    throw new Error(`Expo rejected push: ${ticket?.details?.error ?? 'unknown error'}`);
  }
  return ticket;
}

export function createPushGateway({
  statePath,
  expoAccessToken,
  sendExpo = (message) => defaultExpoSender(expoAccessToken, message),
  clock = () => Date.now(),
  logger = console,
}) {
  if (!statePath) throw new Error('statePath is required');
  if (!expoAccessToken && sendExpo.length === 1) {
    throw new Error('EXPO_ACCESS_TOKEN is required');
  }

  const registrations = readState(statePath);
  const pending = new Map();
  const startAttempts = new Map();
  const sendAttempts = new Map();
  const lastSentAt = new Map();
  const recentEvents = new Map();

  function cleanup(now) {
    for (const [id, item] of pending) {
      if (item.expiresAt <= now) pending.delete(id);
    }
    for (const [key, sentAt] of recentEvents) {
      if (now - sentAt >= EVENT_TTL_MS) recentEvents.delete(key);
    }
  }

  async function handle(request, reply) {
    const now = clock();
    cleanup(now);
    const url = new URL(request.url ?? '/', 'http://gateway.local');

    if (request.method === 'GET' && url.pathname === '/health') {
      response(reply, 200, { ok: true });
      return;
    }

    if (request.method === 'POST' && url.pathname === '/v1/registrations/start') {
      const body = await readJson(request);
      const registrationId = safeString(body.registration_id, 64);
      const pushToken = safeString(body.push_token, 256);
      const secret = safeString(body.secret, 128);
      if (
        !REGISTRATION_ID_RE.test(registrationId)
        || !PUSH_TOKEN_RE.test(pushToken)
        || !HEX_32_RE.test(secret)
      ) {
        response(reply, 400, { error: 'invalid registration request' });
        return;
      }

      const tokenHash = sha256(pushToken);
      const address = clientAddress(request);
      if (
        !allowWithinWindow(startAttempts, `token:${tokenHash}`, now, START_WINDOW_MS, START_LIMIT)
        || !allowWithinWindow(startAttempts, `ip:${address}`, now, START_WINDOW_MS, START_LIMIT * 5)
      ) {
        response(reply, 429, { error: 'registration rate limit exceeded' });
        return;
      }

      const challenge = randomBytes(32).toString('hex');
      pending.set(registrationId, {
        tokenHash,
        secretHash: sha256(secret),
        challengeHash: sha256(challenge),
        expiresAt: now + CHALLENGE_TTL_MS,
      });

      try {
        await sendExpo({
          to: pushToken,
          priority: 'high',
          ttl: Math.floor(CHALLENGE_TTL_MS / 1000),
          data: {
            type: 'vmux.push.verify',
            registration_id: registrationId,
            challenge,
          },
          _contentAvailable: true,
        });
      } catch (error) {
        pending.delete(registrationId);
        logger.error(`push registration delivery failed: ${error.message}`);
        response(reply, 502, { error: 'verification delivery failed' });
        return;
      }

      response(reply, 202, { expires_in: Math.floor(CHALLENGE_TTL_MS / 1000) });
      return;
    }

    if (request.method === 'POST' && url.pathname === '/v1/registrations/confirm') {
      const body = await readJson(request);
      const registrationId = safeString(body.registration_id, 64);
      const challenge = safeString(body.challenge, 128);
      const item = pending.get(registrationId);
      if (!item || item.expiresAt <= now || !constantTimeHexEqual(item.challengeHash, sha256(challenge))) {
        response(reply, 401, { error: 'invalid or expired verification' });
        return;
      }
      pending.delete(registrationId);
      registrations.set(item.tokenHash, {
        tokenHash: item.tokenHash,
        secretHash: item.secretHash,
        createdAt: now,
      });
      persistState(statePath, registrations);
      response(reply, 204);
      return;
    }

    if (request.method === 'DELETE' && url.pathname === '/v1/registrations/me') {
      const body = await readJson(request);
      const pushToken = safeString(body.push_token, 256);
      const secret = bearer(request);
      const tokenHash = sha256(pushToken);
      const item = registrations.get(tokenHash);
      if (
        !PUSH_TOKEN_RE.test(pushToken)
        || !HEX_32_RE.test(secret)
        || !item
        || !constantTimeHexEqual(item.secretHash, sha256(secret))
      ) {
        response(reply, 401, { error: 'unauthorized' });
        return;
      }
      registrations.delete(tokenHash);
      persistState(statePath, registrations);
      response(reply, 204);
      return;
    }

    if (request.method === 'POST' && url.pathname === '/v1/registrations/check') {
      const body = await readJson(request);
      const pushToken = safeString(body.push_token, 256);
      const secret = bearer(request);
      const tokenHash = sha256(pushToken);
      const item = registrations.get(tokenHash);
      if (
        !PUSH_TOKEN_RE.test(pushToken)
        || !HEX_32_RE.test(secret)
        || !item
        || !constantTimeHexEqual(item.secretHash, sha256(secret))
      ) {
        response(reply, 401, { error: 'unauthorized' });
        return;
      }
      response(reply, 204);
      return;
    }

    if (request.method === 'POST' && url.pathname === '/v1/notifications') {
      const body = await readJson(request);
      const pushToken = safeString(body.push_token, 256);
      const secret = bearer(request);
      const eventId = safeString(body.event_id, 128);
      const tokenHash = sha256(pushToken);
      const item = registrations.get(tokenHash);
      if (
        !PUSH_TOKEN_RE.test(pushToken)
        || !HEX_32_RE.test(secret)
        || !eventId
        || !item
        || !constantTimeHexEqual(item.secretHash, sha256(secret))
      ) {
        response(reply, 401, { error: 'unauthorized' });
        return;
      }

      const eventKey = `${tokenHash}:${sha256(eventId)}`;
      if (recentEvents.has(eventKey)) {
        response(reply, 202, { duplicate: true });
        return;
      }
      const previous = lastSentAt.get(tokenHash) ?? 0;
      if (
        now - previous < MIN_SEND_GAP_MS
        || !allowWithinWindow(sendAttempts, tokenHash, now, SEND_WINDOW_MS, SEND_LIMIT)
      ) {
        response(reply, 429, { error: 'notification rate limit exceeded' });
        return;
      }

      try {
        await sendExpo({
          to: pushToken,
          title: 'An agent needs you',
          body: 'Open vmux Remote to continue.',
          sound: 'default',
          priority: 'high',
          ttl: 10 * 60,
          data: {
            type: 'vmux.agent.attention',
            event_id: eventId,
            workspace: safeString(body.workspace, 128),
            pane: safeString(body.pane, 128),
          },
          channelId: 'agents',
        });
      } catch (error) {
        logger.error(`notification delivery failed: ${error.message}`);
        response(reply, 502, { error: 'notification delivery failed' });
        return;
      }

      recentEvents.set(eventKey, now);
      lastSentAt.set(tokenHash, now);
      response(reply, 202, { accepted: true });
      return;
    }

    response(reply, 404, { error: 'not found' });
  }

  return createServer((request, reply) => {
    handle(request, reply).catch((error) => {
      logger.error(`gateway request failed: ${error.message}`);
      if (!reply.headersSent) response(reply, error.status ?? 500, { error: 'request failed' });
      else reply.destroy();
    });
  });
}

async function main() {
  const host = process.env.VMUX_PUSH_LISTEN ?? '127.0.0.1';
  const port = Number(process.env.VMUX_PUSH_PORT ?? 4180);
  const statePath = process.env.VMUX_PUSH_STATE
    ?? join(homedir(), '.local', 'state', 'vmux-push', 'registrations.json');
  const expoTokenFile = process.env.EXPO_ACCESS_TOKEN_FILE
    ?? (process.env.CREDENTIALS_DIRECTORY
      ? join(process.env.CREDENTIALS_DIRECTORY, 'expo-access-token')
      : '');
  const expoAccessToken = expoTokenFile
    ? readFileSync(expoTokenFile, 'utf8').trim()
    : (process.env.EXPO_ACCESS_TOKEN ?? '').trim();
  if (!expoAccessToken) throw new Error('EXPO_ACCESS_TOKEN is required');
  if (!Number.isInteger(port) || port < 1 || port > 65535) throw new Error('invalid port');

  const server = createPushGateway({ statePath, expoAccessToken });
  server.listen(port, host, () => {
    console.log(`vmux push gateway listening on http://${host}:${port}`);
  });
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch((error) => {
    console.error(error.message);
    process.exitCode = 1;
  });
}
