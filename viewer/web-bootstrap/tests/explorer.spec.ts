import {expect, test, type Page} from "@playwright/test";
import {execFile, spawn, type ChildProcessWithoutNullStreams} from "node:child_process";
import {mkdtemp, rm} from "node:fs/promises";
import {tmpdir} from "node:os";
import path from "node:path";
import {fileURLToPath} from "node:url";
import {promisify} from "node:util";

const execute = promisify(execFile);
const testDirectory = path.dirname(fileURLToPath(import.meta.url));
const repository = path.resolve(testDirectory, "../../..");
const executable = process.env.ECOSCOPE_BIN ?? path.join(repository, "target/debug/ecoscope");

let stateRoot: string;
let explorer: ChildProcessWithoutNullStreams;
let explorerUrl: string;
let pointViewId: string;

test.beforeAll(async () => {
  stateRoot = await mkdtemp(path.join(tmpdir(), "ecoscope-browser-smoke-"));
  const environment = demoEnvironment();
  const {stdout} = await execute(executable, ["demo", "synthetic", "--no-open"], {
    env: environment,
    timeout: 90_000
  });
  const demo = JSON.parse(stdout);
  const pointView = await execute(
    executable,
    ["create-view", "--name", "Point selection smoke", demo.dataset_ids[0]],
    {env: environment, timeout: 30_000}
  );
  pointViewId = JSON.parse(pointView.stdout).view_id;
  explorerUrl = await startExplorer(demo.view_id);
});

test.afterAll(async () => {
  await stopExplorer();
  if (stateRoot) await rm(stateRoot, {recursive: true, force: true});
});

test("renders LiDAR and hyperspectral views and records real Rerun picks", async ({page}) => {
  await page.goto(explorerUrl);
  await loadRenderedExplorer(page);

  const cubeSelection = await clickUntilSelection(
    page,
    "cube_pixel",
    [0.51, 0.80],
    [0.15, 0.25, 0.35, 0.1, 0.4]
  );
  expect(cubeSelection.selection.array_path).toBe("/EcoScope/Reflectance");
  expect(cubeSelection.selection.x).toBeGreaterThanOrEqual(0);
  expect(cubeSelection.selection.y).toBeGreaterThanOrEqual(0);

  await stopExplorer();
  explorerUrl = await startExplorer(pointViewId);
  await page.goto(explorerUrl);
  await loadRenderedExplorer(page);
  const pointSelection = await clickUntilSelection(
    page,
    "point_set",
    [0.22, 0.88],
    [0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8]
  );
  expect(pointSelection.selection.spatial_query.source_index_verified).toBe(true);
  expect(pointSelection.selection.spatial_query.source_indices).toHaveLength(1);
});

async function startExplorer(viewId: string): Promise<string> {
  explorer = spawn(executable, ["serve", viewId, "--port", "0"], {
    env: demoEnvironment(),
    stdio: ["ignore", "pipe", "pipe"]
  });
  return new Promise<string>((resolve, reject) => {
    const timeout = setTimeout(() => reject(new Error("explorer URL was not emitted")), 60_000);
    let output = "";
    explorer.stdout.on("data", chunk => {
      output += chunk.toString();
      const match = output.match(/EcoScope explorer: (http:\/\/[^\s]+)/);
      if (match) {
        clearTimeout(timeout);
        resolve(match[1]);
      }
    });
    explorer.stderr.on("data", chunk => {
      process.stderr.write(chunk);
    });
    explorer.once("exit", code => {
      clearTimeout(timeout);
      reject(new Error(`explorer exited before startup with ${code}`));
    });
  });
}

async function loadRenderedExplorer(page: Page): Promise<void> {
  await expect(page.locator("#status")).toContainText("Rerun semantic bridge active");
  await expect(page.locator("#viewer canvas")).toBeVisible();
  await page.waitForTimeout(2_000);
  await expect(page.locator("#viewer")).not.toContainText("Rerun has crashed");
  await expect(page.locator("#view-details")).toContainText("Layers");
}

async function stopExplorer(): Promise<void> {
  if (!explorer || explorer.exitCode !== null) return;
  explorer.kill("SIGINT");
  await new Promise<void>(resolve => {
    const timeout = setTimeout(resolve, 5_000);
    explorer.once("exit", () => {
      clearTimeout(timeout);
      resolve();
    });
  });
}

async function clickUntilSelection(
  page: Page,
  selectionType: string,
  horizontalRegion: [number, number],
  verticalFractions: number[]
): Promise<any> {
  const canvas = page.locator("#viewer canvas");
  const box = await canvas.boundingBox();
  if (!box) throw new Error("Rerun canvas has no bounding box");
  for (const yFraction of verticalFractions) {
    for (let column = 1; column <= 7; column += 1) {
      const xFraction = horizontalRegion[0] +
        (horizontalRegion[1] - horizontalRegion[0]) * (column / 8);
      await page.mouse.click(
        box.x + box.width * xFraction,
        box.y + box.height * yFraction,
        {delay: 100}
      );
      await page.waitForTimeout(350);
      const record = await latestSelection(page);
      if (record?.selection?.type === selectionType) return record;
    }
  }
  throw new Error(`no ${selectionType} selection was produced by the rendered Rerun view`);
}

async function latestSelection(page: Page): Promise<any> {
  return page.evaluate(async () => {
    const token = new URLSearchParams(window.location.search).get("token");
    const response = await fetch(`/api/selection?token=${encodeURIComponent(token ?? "")}`);
    return response.json();
  });
}

function demoEnvironment(): NodeJS.ProcessEnv {
  return {
    ...process.env,
    ECOSCOPE_DATA_DIR: path.join(stateRoot, "data"),
    ECOSCOPE_CACHE_DIR: path.join(stateRoot, "cache"),
    ECOSCOPE_WEB_DIST: path.join(repository, "viewer/web-bootstrap/dist")
  };
}
