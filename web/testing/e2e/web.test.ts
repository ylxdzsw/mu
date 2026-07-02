import assert from "node:assert/strict";

import { chromium } from "@playwright/test";
import type { Page } from "@playwright/test";

import { startHarness, type Harness } from "./harness.ts";

function launchBrowser() {
  return chromium.launch({
    executablePath: process.env.CHROME_PATH || "/usr/bin/chromium",
    headless: true,
    args: ["--no-sandbox"],
  });
}

async function withBrowserSession(run: (context: { harness: Harness; page: Page }) => Promise<void>) {
  const harness = await startHarness();
  const browser = await launchBrowser();
  const page = await browser.newPage();

  try {
    await run({ harness, page });
  } finally {
    await browser.close();
    await harness.close();
  }
}

async function verifyModelPicker() {
  await withBrowserSession(async ({ harness, page }) => {
    await page.goto(harness.baseUrl, { waitUntil: "load" });
    await page.waitForFunction(
      () => document.querySelector('[aria-label="Model"]')?.textContent?.includes("fake/fake-model"),
    );
    await page.getByText("What should mu help with next?").waitFor({ state: "visible" });
    assert.equal(
      (await page.getByRole("button", { name: "Model" }).textContent())?.trim(),
      "fake/fake-model",
    );
  });
}

async function verifyPromptSubmission() {
  await withBrowserSession(async ({ harness, page }) => {
    await page.goto(harness.baseUrl, { waitUntil: "load" });

    const prompt = "hello from browser";
    await page.getByRole("textbox", { name: "Prompt" }).fill(prompt);
    await page.getByRole("button", { name: "Send prompt" }).click();

    await page
      .locator(".conversation-card-body[data-mono='false']")
      .filter({ hasText: `Fake response to: ${prompt}` })
      .waitFor({ state: "visible" });

    assert.equal(
      harness.providerRequests.some((request) => JSON.stringify(request).includes(prompt)),
      true,
    );
  });
}

async function main() {
  await verifyModelPicker();
  console.log("ok - loads split frontend assets and status-driven model picker");
  await verifyPromptSubmission();
  console.log("ok - submits a prompt through the standalone web server against the fake provider");
}

await main();
