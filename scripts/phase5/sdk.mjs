import assert from 'node:assert/strict';
import { Client, StreamableHTTPClientTransport } from '@modelcontextprotocol/client';

const [url, repository] = process.argv.slice(2);
const revision = '2026-07-28';
const client = new Client(
  { name: 'phase5-typescript-sdk', version: '1.0.0' },
  { versionNegotiation: { mode: { pin: revision } } },
);
await client.connect(new StreamableHTTPClientTransport(new URL(url)));
try {
  assert.deepEqual((await client.discover()).supportedVersions, [revision]);
  const { tools } = await client.listTools();
  assert.equal(tools.length, 12);
  const call = async (name, args) => {
    const result = await client.callTool({ name, arguments: args });
    assert.notEqual(result.isError, true, JSON.stringify(result));
    assert.deepEqual(JSON.parse(result.content[0].text), result.structuredContent);
    return result.structuredContent;
  };
  const { context } = await call('git.resolve', { repository_id: repository, selector: { kind: 'default' } });
  const path = { encoding: 'utf8', value: 'data/note' };
  await call('repository.search', {});
  await call('repository.info', { repository_id: repository });
  await call('git.refs', { repository_id: repository });
  await call('git.ls_tree', { context });
  await call('git.find', { context, pattern: { kind: 'literal', value: path } });
  await call('git.read', { context, path });
  assert.equal((await call('git.rg', { context, pattern: '資料 match', literal: true })).matches.length, 2);
  await call('git.log', { context });
  await call('git.show', { context });
  await call('git.diff', { base: context, head: context });
  await call('git.blame', { context, path });
  console.log('Pinned TypeScript SDK discovered, listed and called all twelve tools');
} finally {
  await client.close();
}
