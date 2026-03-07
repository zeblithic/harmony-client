export class RingBuffer<T> {
  private buffer: (T | undefined)[];
  private head = 0;
  private count = 0;
  readonly capacity: number;

  constructor(capacity: number) {
    this.capacity = Math.max(1, Math.floor(capacity));
    this.buffer = new Array(this.capacity);
  }

  get length(): number {
    return this.count;
  }

  push(item: T): void {
    const idx = (this.head + this.count) % this.capacity;
    this.buffer[idx] = item;
    if (this.count < this.capacity) {
      this.count++;
    } else {
      this.head = (this.head + 1) % this.capacity;
    }
  }

  peek(): T | undefined {
    if (this.count === 0) return undefined;
    return this.buffer[(this.head + this.count - 1) % this.capacity];
  }

  clear(): void {
    this.buffer = new Array(this.capacity);
    this.head = 0;
    this.count = 0;
  }

  toArray(): T[] {
    const result: T[] = [];
    for (let i = 0; i < this.count; i++) {
      result.push(this.buffer[(this.head + i) % this.capacity] as T);
    }
    return result;
  }

  [Symbol.iterator](): Iterator<T> {
    let i = 0;
    const self = this;
    return {
      next(): IteratorResult<T> {
        if (i < self.count) {
          const value = self.buffer[(self.head + i) % self.capacity] as T;
          i++;
          return { value, done: false };
        }
        return { value: undefined, done: true };
      },
    };
  }
}
