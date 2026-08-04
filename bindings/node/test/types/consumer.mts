import {
  NativeStore,
  TurnDbError,
  capabilities,
  type Attr,
  type ScanPage,
  type WriteOp,
} from '@turndb/native';

const member: Attr = { name: 'member.key', kind: 'string', stringValue: 'alice' };
const operation: WriteOp = {
  kind: 'put',
  id: 'member/alice/0001/activity',
  attrs: [member],
};

async function consumer(directory: string): Promise<ScanPage> {
  const store = await NativeStore.open(directory);
  try {
    await store.write([operation], true);
    return await store.scan({
      attrs: ['member.key'],
      predicates: [{ kind: 'attr', op: 'eq', value: member }],
    });
  } catch (error) {
    if (error instanceof TurnDbError) console.error(error.code);
    throw error;
  } finally {
    await store.close();
  }
}

void capabilities;
void consumer;
