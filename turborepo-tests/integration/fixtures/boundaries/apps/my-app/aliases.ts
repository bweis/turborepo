// tsconfig path alias into a workspace package that is NOT a dependency should fail
import { ship } from "@utils/index";
// tsconfig path alias to a directory outside of every workspace package leaves the package
import { compass } from "@shared/compass";
