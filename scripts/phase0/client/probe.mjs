import assert from 'node:assert/strict';
import { Client, StreamableHTTPClientTransport } from '@modelcontextprotocol/client';

const revision = '2026-07-28';
const client = new Client(
  { name: 'phase0-typescript-sdk', version: '1.0.0' },
  { versionNegotiation: { mode: { pin: revision } } },
);
await client.connect(new StreamableHTTPClientTransport(new URL(process.argv[2])));
try {
  assert.equal(client.getProtocolEra(), 'modern');
  const discovery = await client.discover();
  assert.deepEqual(discovery.supportedVersions, [revision]);
  const { tools } = await client.listTools();
  assert.deepEqual(tools.map(({ name }) => name), ['phase0_read_fixture']);
  assert.deepEqual(tools[0].inputSchema, {
    type: 'object', properties: {}, additionalProperties: false,
  });
  const result = await client.callTool({ name: tools[0].name, arguments: {} });
  assert.notEqual(result.isError, true);
  assert.deepEqual(result.content, [{ type: 'text', text: 'phase-zero fixture' }]);
  console.log('@modelcontextprotocol/client 2.4.0 discovered, listed and called the fixture');
} finally {
  await client.close();
}
