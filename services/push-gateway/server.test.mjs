import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { mkdtemp } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, test } from 'node:test';

import { createPushGateway } from './server.mjs';

const servers = [];
afterEach(async () => {
  await Promise.all(servers.splice(0).map((server) => new Promise((resolve) => server.close(resolve))));
});

async function fixture() {
  const directory = await mkdtemp(join(tmpdir(), 'vmux-push-test-'));
  const sent = [];
  let now = 1_800_000_000_000;
  const server = createPushGateway({
    statePath: join(directory, 'registrations.json'),
    expoAccessToken: 'test-only',
    sendExpo: async (message) => {
      sent.push(message);
      return { status: 'ok', id: `ticket-${sent.length}` };
    },
    clock: () => now,
    logger: { error() {} },
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  servers.push(server);
  const address = server.address();
  const base = `http://127.0.0.1:${address.port}`;
  return { base, directory, sent, advance: (milliseconds) => { now += milliseconds; } };
}

async function call(base, path, { method = 'POST', token, body } = {}) {
  return fetch(base + path, {
    method,
    headers: {
      ...(token ? { Authorization: `Bearer ${token}` } : {}),
      'Content-Type': 'application/json',
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
}

test('registers only after proving delivery and stores hashes only', async () => {
  const { base, directory, sent } = await fixture();
  const pushToken = 'ExponentPushToken[secure-device-token]';
  const secret = 'ab'.repeat(32);
  const registrationId = 'cd'.repeat(16);

  const start = await call(base, '/v1/registrations/start', {
    body: { registration_id: registrationId, push_token: pushToken, secret },
  });
  assert.equal(start.status, 202);
  assert.equal(sent.length, 1);
  assert.equal(sent[0].to, pushToken);
  assert.equal(sent[0].title, undefined);
  const challenge = sent[0].data.challenge;

  const wrong = await call(base, '/v1/registrations/confirm', {
    body: { registration_id: registrationId, challenge: 'ef'.repeat(32) },
  });
  assert.equal(wrong.status, 401);

  const confirm = await call(base, '/v1/registrations/confirm', {
    body: { registration_id: registrationId, challenge },
  });
  assert.equal(confirm.status, 204);

  const validCheck = await call(base, '/v1/registrations/check', {
    token: secret,
    body: { push_token: pushToken },
  });
  assert.equal(validCheck.status, 204);
  const invalidCheck = await call(base, '/v1/registrations/check', {
    token: 'ef'.repeat(32),
    body: { push_token: pushToken },
  });
  assert.equal(invalidCheck.status, 401);

  const state = readFileSync(join(directory, 'registrations.json'), 'utf8');
  assert.equal(state.includes(pushToken), false);
  assert.equal(state.includes(secret), false);
  assert.match(state, /token_hash/);
  assert.match(state, /secret_hash/);
});

test('requires the token-secret pair and constructs fixed safe notification text', async () => {
  const { base, sent, advance } = await fixture();
  const pushToken = 'ExponentPushToken[another-secure-device]';
  const secret = '12'.repeat(32);
  const registrationId = '34'.repeat(16);
  await call(base, '/v1/registrations/start', {
    body: { registration_id: registrationId, push_token: pushToken, secret },
  });
  await call(base, '/v1/registrations/confirm', {
    body: { registration_id: registrationId, challenge: sent[0].data.challenge },
  });
  sent.length = 0;

  const denied = await call(base, '/v1/notifications', {
    token: '56'.repeat(32),
    body: { push_token: pushToken, event_id: 'evt-1' },
  });
  assert.equal(denied.status, 401);

  const accepted = await call(base, '/v1/notifications', {
    token: secret,
    body: {
      push_token: pushToken,
      event_id: 'evt-1',
      title: 'Fake security warning',
      body: 'Run an attacker command',
      workspace: 'agents',
      pane: 'pane-1',
    },
  });
  assert.equal(accepted.status, 202);
  assert.equal(sent.length, 1);
  assert.equal(sent[0].title, 'An agent needs you');
  assert.equal(sent[0].body, 'Open vmux Remote to continue.');
  assert.equal(sent[0].data.workspace, 'agents');

  advance(5_000);
  const duplicate = await call(base, '/v1/notifications', {
    token: secret,
    body: { push_token: pushToken, event_id: 'evt-1' },
  });
  assert.equal(duplicate.status, 202);
  assert.equal((await duplicate.json()).duplicate, true);
  assert.equal(sent.length, 1);
});

test('unregister requires both credentials and prevents later sends', async () => {
  const { base, sent } = await fixture();
  const pushToken = 'ExponentPushToken[device-to-remove]';
  const secret = '78'.repeat(32);
  const registrationId = '90'.repeat(16);
  await call(base, '/v1/registrations/start', {
    body: { registration_id: registrationId, push_token: pushToken, secret },
  });
  await call(base, '/v1/registrations/confirm', {
    body: { registration_id: registrationId, challenge: sent[0].data.challenge },
  });

  const removed = await call(base, '/v1/registrations/me', {
    method: 'DELETE',
    token: secret,
    body: { push_token: pushToken },
  });
  assert.equal(removed.status, 204);
  const denied = await call(base, '/v1/notifications', {
    token: secret,
    body: { push_token: pushToken, event_id: 'after-delete' },
  });
  assert.equal(denied.status, 401);
});
