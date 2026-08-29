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
const executable = process.env.WILDDATUM_BIN ?? path.join(repository, "target/debug/wilddatum");

let stateRoot: string;
let explorer: ChildProcessWithoutNullStreams;
let explorerUrl: string;
let pointViewId: string;
let profileViewId: string;

test.beforeAll(async () => {
  stateRoot = await mkdtemp(path.join(tmpdir(), "wilddatum-browser-smoke-"));
  const environment = demoEnvironment();
  const {stdout} = await execute(executable, ["demo", "synthetic", "--no-open"], {
    env: environment,
    timeout: 90_000
  });
  const demo = JSON.parse(stdout);
  const suggestionOutput = await execute(
    executable,
    ["suggest-views", ...demo.dataset_ids],
    {env: environment, timeout: 30_000}
  );
  const suggestion = JSON.parse(suggestionOutput.stdout).suggestions.find(
    (candidate: any) => candidate.recipe === "point_cloud_spectral_cube_v1"
  );
  if (!suggestion) throw new Error("synthetic demo did not produce a multimodal suggestion");
  const acceptedOutput = await execute(
    executable,
    [
      "create-suggested-view",
      suggestion.suggestion_id,
      "--name",
      "Accepted multimodal workspace",
      ...demo.dataset_ids
    ],
    {env: environment, timeout: 30_000}
  );
  const acceptedView = JSON.parse(acceptedOutput.stdout);
  const pointView = await execute(
    executable,
    ["create-view", "--name", "Point selection smoke", demo.dataset_ids[0]],
    {env: environment, timeout: 30_000}
  );
  pointViewId = JSON.parse(pointView.stdout).view_id;
  const profileDemo = await execute(
    executable,
    ["demo", "profile-trajectory", "--no-open"],
    {env: environment, timeout: 90_000}
  );
  profileViewId = JSON.parse(profileDemo.stdout).view_id;
  explorerUrl = await startExplorer(acceptedView.view_id);
});

test.afterAll(async () => {
  await stopExplorer();
  if (stateRoot) await rm(stateRoot, {recursive: true, force: true});
});

test("renders LiDAR and hyperspectral views and records real Rerun picks", async ({page}) => {
  await page.goto(explorerUrl);
  await loadRenderedExplorer(page);
  await expect(page.locator("#view-details")).toContainText("EcoViewSpec v2");
  await expect(page.locator("#view-details")).toContainText("Panels");
  await expect(page.locator("#view-details")).toContainText("3");
  await expect(page.locator("#view-details")).toContainText("Links");

  const cubeSelection = await clickUntilSelection(
    page,
    "cube_pixel",
    [0.51, 0.80],
    [0.15, 0.25, 0.35, 0.1, 0.4]
  );
  expect(cubeSelection.selection.array_path).toBe("/WildDatum/Reflectance");
  expect(cubeSelection.selection.x).toBeGreaterThanOrEqual(0);
  expect(cubeSelection.selection.y).toBeGreaterThanOrEqual(0);
  await expect(page.locator("#selection")).toContainText("cube_pixel_to_spectrum");
  await expect(page.locator("#selection")).toContainText('"status": "resolved"');
  await expect(page.locator("#selection")).toContainText("wavelength_nm");

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

test("profile trajectory contract exposes a real Rerun instance pick", async ({page}) => {
  await stopExplorer();
  explorerUrl = await startExplorer(profileViewId);
  await page.goto(explorerUrl);
  await loadRenderedExplorer(page);

  const mapPick = await clickNearObservation(
    page,
    "map_observations",
    0.595,
    0.275,
    15
  );
  expect(mapPick.item.instance_id).toBe(15);
  expect(mapPick.record.selection.type).toBe("rows");
  expect(mapPick.record.selection.dataset_id).toBeTruthy();
  expect(mapPick.record.selection.row_count).toBe(1);
  expect(mapPick.record.selection.predicate).toEqual({
    entity_path: mapPick.item.entity_path,
    instance_id: 15,
    mapping_kind: "source_row_index",
    rerun_version: "0.36.2"
  });
  expect(mapPick.record.selection.predicate).not.toHaveProperty("source_index");
  expect(mapPick.record.selection.predicate).not.toHaveProperty("source_indices");
  expect(mapPick.record.selection.predicate).not.toHaveProperty("source_index_verified");

  const queried = await execute(
    executable,
    ["query-selection", mapPick.record.selection_id],
    {env: demoEnvironment(), timeout: 30_000}
  );
  const exactRow = JSON.parse(queried.stdout);
  expect(exactRow.preview.rows[0].source_index).toBe(15);
  expect(exactRow.preview.rows[0].values.platform_number).toBe("FLOAT_SYNTH_001");
  expect(exactRow.preview.rows[0].values.cycle_number).toBe("2");
  expect(exactRow.preview.rows[0].values.pres).toBe("700");
  expect(exactRow.preview.rows[0].values.temp_adjusted).toBe("7.94");
  expect(exactRow.preview.rows[0].values.temp_adjusted_qc).toBe("1");

  const profilePick = await clickNearObservation(
    page,
    "profile_observations",
    0.8,
    0.63
  );
  expect(Number.isInteger(profilePick.item.instance_id)).toBe(true);
  expect(profilePick.item.instance_id).toBeGreaterThanOrEqual(0);
  expect(profilePick.item.instance_id).toBeLessThan(16);
  expect(profilePick.record.selection.type).toBe("rows");
  expect(profilePick.record.selection.predicate.instance_id).toBe(profilePick.item.instance_id);
  expect(profilePick.record.selection.predicate.entity_path).toBe(profilePick.item.entity_path);
  await expect(page.locator("#viewer")).not.toContainText("Rerun has crashed");
  if (process.env.WILDDATUM_PROFILE_SCREENSHOT) {
    await page.screenshot({
      path: process.env.WILDDATUM_PROFILE_SCREENSHOT,
      fullPage: true
    });
  }
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
      const match = output.match(/WildDatum explorer: (http:\/\/[^\s]+)/);
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

async function clickNearObservation(
  page: Page,
  entitySuffix: "map_observations" | "profile_observations",
  xFraction: number,
  yFraction: number,
  expectedInstance?: number
): Promise<{
  record: any;
  item: {entity_path: string; instance_id: number};
}> {
  const canvas = page.locator("#viewer canvas");
  const box = await canvas.boundingBox();
  if (!box) throw new Error("Rerun canvas has no bounding box");
  const offsets = [0, -5, 5, -10, 10, -15, 15];
  for (const yOffset of offsets) {
    for (const xOffset of offsets) {
      for (let attempt = 0; attempt < 2; attempt += 1) {
        await page.mouse.click(
          box.x + box.width * xFraction + xOffset,
          box.y + box.height * yFraction + yOffset,
          {delay: 80}
        );
        await page.waitForTimeout(250);
        const record = await latestSelection(page);
        const item = record?.summary?.raw_event?.items?.find((candidate: any) =>
          candidate?.type === "entity" &&
          String(candidate.entity_path).endsWith(`/${entitySuffix}`) &&
          Number.isInteger(candidate.instance_id) &&
          (expectedInstance === undefined || candidate.instance_id === expectedInstance)
        );
        if (item) return {record, item};
      }
    }
  }
  throw new Error(
    `no ${entitySuffix} instance${expectedInstance === undefined ? "" : ` ${expectedInstance}`} was picked`
  );
}

function demoEnvironment(): NodeJS.ProcessEnv {
  return {
    ...process.env,
    WILDDATUM_DATA_DIR: path.join(stateRoot, "data"),
    WILDDATUM_CACHE_DIR: path.join(stateRoot, "cache"),
    WILDDATUM_WEB_DIST: path.join(repository, "viewer/web-bootstrap/dist")
  };
}
