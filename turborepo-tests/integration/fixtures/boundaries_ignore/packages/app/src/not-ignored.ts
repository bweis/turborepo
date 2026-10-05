// Not ignored: doesn't match any `boundaries.ignore` glob. The specifier
// differs from the one in `routeTree.gen.ts` so the snapshot shows which file
// was reported.
import { lib } from "../../lib/src/index.ts";

export const notIgnored = lib;
