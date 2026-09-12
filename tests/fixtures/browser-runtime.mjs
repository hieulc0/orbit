// A real browser talking to a disposable Orbit server; no mocked API routes.
import { createRequire } from 'node:module';
const require = createRequire(process.env.ORBIT_UI_PACKAGE);
const { chromium } = require('@playwright/test');
const browser = await chromium.launch();
try {
  const page = await browser.newPage({ viewport: { width: 1440, height: 1000 } });
  const errors = [];
  page.on('pageerror', error => errors.push(error.message));
  await page.goto(`${process.env.ORBIT_URL}/console/`);
  await page.getByLabel('Operator token').fill(process.env.ORBIT_TOKEN);
  await page.getByRole('button', { name: 'Connect', exact: true }).click();
  await page.getByRole('heading', { name: 'Runs', exact: true }).waitFor();
  await page.getByRole('button', { name: 'New definition', exact: true }).click();
  await page.getByRole('button', { name: 'Select step review', exact: true }).click();
  const timeout = page.getByLabel('Step timeout_seconds', { exact: true });
  await timeout.fill('120'); await timeout.blur();
  await page.getByRole('button', { name: 'Validate', exact: true }).click();
  const submission = page.waitForResponse(response => response.url().endsWith('/runs') && response.request().method() === 'POST');
  page.once('dialog', dialog => dialog.accept());
  await page.getByRole('button', { name: 'Submit run', exact: true }).click();
  const result = await (await submission).json();
  if (!result.run_id) throw new Error('Run submission did not return an ID');
  await page.getByLabel('Waiting step').selectOption('review');
  await page.getByLabel('Comment', { exact: true }).fill('Disposable browser qualification approved.');
  const decision = page.waitForResponse(response => response.url().endsWith('/approvals'));
  page.once('dialog', dialog => dialog.accept());
  await page.getByRole('button', { name: 'Record decision', exact: true }).click();
  if (!(await decision).ok()) throw new Error('Browser approval rejected');
  await page.locator('h2 .badge.succeeded').waitFor();
  await page.screenshot({ path: process.env.ORBIT_BROWSER_SCREENSHOT, fullPage: true });
  if (errors.length) throw new Error(errors.join('\n'));
  const stored = await page.evaluate(() => localStorage.length + sessionStorage.length);
  if (stored !== 0) throw new Error('Unexpected persistent browser storage');
  console.log(JSON.stringify({ run_id: result.run_id, state: 'SUCCEEDED', browser_errors: errors }));
} finally { await browser.close(); }
