import { uploadUrl } from "@repo/storage";

export function Button(props: { label: string }) {
  return { type: "button", props, avatar: uploadUrl("avatar.png") };
}
