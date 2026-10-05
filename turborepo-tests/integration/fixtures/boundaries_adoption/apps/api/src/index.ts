import { query } from "@repo/db";
import { formatDate } from "@repo/utils";
import { z } from "zod";
// @boundaries-ignore the api serves the web app's static route manifest
import manifest from "../../web/src/routes/manifest.json";

export const schema = z.object({ at: z.string() });

export async function handler() {
  return { manifest, now: formatDate(await query("select now()")) };
}
