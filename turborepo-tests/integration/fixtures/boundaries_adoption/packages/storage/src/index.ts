import { S3Client } from "@aws-sdk/client-s3";

export const client = new S3Client({});

export function uploadUrl(key: string) {
  return `https://uploads.example.com/${key}`;
}
