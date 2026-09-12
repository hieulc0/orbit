import { test, expect, type Page } from '@playwright/test';
import { parse } from 'yaml';
import { starter } from '../src/Editor';

const runId = '12345678-1234-4234-8234-123456789012';
const token = 'browser-fixture-token-not-a-secret';
async function fixture(page: Page) {
  const calls: { path: string; body: any }[] = [];
  let state = 'RUNNING';
  const definition = parse(starter);
  await page.route(/\/(runs|workers|queues|definitions)(\/|\?|$)/, async route => {
    const request = route.request(), path = new URL(request.url()).pathname;
    expect(request.headers().authorization).toBe(`Bearer ${token}`);
    const body = request.postDataJSON();
    if (request.method() === 'POST') calls.push({ path, body });
    let value: any;
    if (path === '/runs' && request.method() === 'GET') value = [{ id: runId, state, created_at: '2026-09-12 10:00:00+00' }];
    else if (path === '/runs') value = { status: 'accepted', run_id: runId };
    else if (path === '/workers') value = [{ id: 'worker-local', last_seen: 'now', idle_seconds: 1, profile: { capabilities: ['agent.run'], capacity: { resources: { cpu_millis: 1000, memory_mib: 128, gpu: 0 } } }, active_attempts: [] }];
    else if (path === '/queues') value = [{ capability: 'agent.run', pool: 'local', ready: 2, active: 1 }];
    else if (path === '/definitions/schema') value = { $defs: { Step: { properties: { uses: { type: 'string' }, needs: {}, timeout_seconds: { type: 'integer' }, max_attempts: { type: 'integer' }, retry_backoff_seconds: { type: 'integer' }, recovery_policy: { type: 'string' }, approval: { type: 'object' }, delay_seconds: { type: 'integer' } } } } };
    else if (path === '/definitions/validate') {
      try { const d = parse(body.source); if (d.steps.review.needs?.includes('review')) throw Error('dependency cycle'); value = { valid: true, definition: d }; }
      catch (error) { await route.fulfill({ status: 400, json: { error: String(error) } }); return; }
    }
    else if (path.endsWith('/events')) value = Number(new URL(request.url()).searchParams.get('after')) > 0 ? [] : [{ sequence: 1, at: '2026-09-12', event: { type: 'RUN_ACCEPTED', actor: 'operator' } }];
    else if (path.endsWith('/cancel')) { state = 'CANCELLED'; value = { status: 'accepted', state }; }
    else if (path.endsWith('/approvals')) { state = 'SUCCEEDED'; value = { status: 'accepted' }; }
    else if (path === `/runs/${runId}`) value = { id: runId, state, plan: { digest: 'a'.repeat(64), definition }, tasks: [{ id: 'task-review', step: 'review', state: state === 'RUNNING' ? 'WAITING' : state, reason: state === 'SUCCEEDED' ? 'signal received' : null, attempts: [], accepted_outputs: [] }], artifacts: [] };
    else throw Error(`Unexpected request ${path}`);
    await route.fulfill({ json: value });
  });
  await page.goto('/console/');
  await page.getByLabel('Operator token').fill(token);
  await page.getByRole('button', { name: 'Connect', exact: true }).click();
  await expect(page.getByRole('heading', { name: 'Runs', exact: true })).toBeVisible();
  return calls;
}

test('operations, timeline, assigned decision, and in-memory credentials', async ({ page }) => {
  const calls = await fixture(page);
  expect(await page.evaluate(() => [localStorage.length, sessionStorage.length])).toEqual([0, 0]);
  await page.getByRole('button', { name: 'Workers', exact: true }).click();
  await expect(page.getByRole('heading', { name: 'worker-local' })).toBeVisible();
  await page.getByRole('button', { name: 'Queues', exact: true }).click();
  await expect(page.getByRole('cell', { name: 'agent.run' })).toBeVisible();
  await page.getByRole('button', { name: 'Runs', exact: true }).click();
  await page.getByRole('button', { name: runId, exact: true }).click();
  await expect(page.locator('summary').filter({ hasText: 'RUN_ACCEPTED' })).toBeVisible();
  await page.getByLabel('Waiting step').selectOption('review');
  await page.getByLabel('Comment', { exact: true }).fill('Reviewed by a human.');
  page.once('dialog', dialog => dialog.accept());
  await page.getByRole('button', { name: 'Record decision' }).click();
  await expect.poll(() => calls.filter(c => c.path.endsWith('/approvals')).length).toBe(1);
  expect(calls.at(-1)?.body).toMatchObject({ step: 'review', approved: true, comment: 'Reviewed by a human.' });
  await expect(page.getByText('signal received', { exact: true })).toHaveClass('muted');
  await page.getByRole('button', { name: 'Disconnect', exact: true }).click();
  await expect(page.getByLabel('Operator token')).toHaveValue('');
});

test('cancellation requires explicit confirmation', async ({ page }) => {
  const calls = await fixture(page);
  await page.getByRole('button', { name: runId, exact: true }).click();
  page.once('dialog', dialog => dialog.dismiss());
  await page.getByRole('button', { name: 'Cancel run', exact: true }).click();
  expect(calls).toHaveLength(0);
  page.once('dialog', dialog => dialog.accept());
  await page.getByRole('button', { name: 'Cancel run', exact: true }).click();
  await expect.poll(() => calls.length).toBe(1);
  await expect(page.getByRole('button', { name: 'Cancel run', exact: true })).toBeDisabled();
});

test('canonical graph editing, schema panels, validation and submit', async ({ page }) => {
  const calls = await fixture(page);
  await page.getByRole('button', { name: 'Definition studio', exact: true }).click();
  await page.getByRole('button', { name: 'Select step review', exact: true }).click();
  const timeout = page.getByLabel('Step timeout_seconds', { exact: true });
  await timeout.fill('240'); await timeout.blur();
  expect(parse(await page.getByLabel('Definition source', { exact: true }).inputValue()).steps.review.timeout_seconds).toBe(240);
  await page.getByLabel('New step ID').fill('after-review');
  await page.getByRole('button', { name: 'Add step', exact: true }).click();
  await page.getByRole('checkbox', { name: 'review', exact: true }).check();
  const source = await page.getByLabel('Definition source', { exact: true }).inputValue();
  expect(parse(source).steps['after-review'].needs).toEqual(['review']);
  await page.getByRole('button', { name: 'Validate', exact: true }).click();
  await expect(page.getByRole('button', { name: 'Submit run', exact: true })).toBeEnabled();
  page.once('dialog', dialog => dialog.accept());
  await page.getByRole('button', { name: 'Submit run', exact: true }).click();
  await expect.poll(() => calls.filter(c => c.path === '/runs').length).toBe(1);
  expect(calls.find(c => c.path === '/runs')?.body.definition).toEqual(parse(source));
});

test('malformed source cannot crash the studio or submit stale validation', async ({ page }) => {
  await fixture(page);
  await page.getByRole('button', { name: 'Definition studio', exact: true }).click();
  await page.getByRole('button', { name: 'Validate', exact: true }).click();
  await expect(page.getByRole('button', { name: 'Submit run', exact: true })).toBeEnabled();
  await page.getByLabel('Definition source', { exact: true }).fill('steps: {bad: {needs: 5}}');
  await expect(page.getByRole('alert')).toContainText('needs must be an array');
  await expect(page.getByRole('button', { name: 'Submit run', exact: true })).toBeDisabled();
});

test('mobile navigation remains operable', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await fixture(page);
  await page.getByRole('button', { name: 'Definition studio', exact: true }).click();
  await expect(page.getByLabel('Definition source', { exact: true })).toBeVisible();
});
