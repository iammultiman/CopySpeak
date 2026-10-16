/**
 * Fragment queue management for streaming playback
 * Handles queuing, processing, and auto-advancement of audio fragments
 */

/** Fragment queued for streaming playback */
export interface QueuedFragment {
  browserReadingId?: string | null;
  audioBase64: string;
  index: number;
  total: number;
  text: string;
  captions?: import("$lib/models/captions").CaptionAlignment | null;
  /** True for the last fragment of the reading. */
  isFinal?: boolean;
}

export interface FragmentQueueHandlers {
  onFragmentPlay: (fragment: QueuedFragment) => Promise<void>;
  onQueueComplete: () => void;
}

export class FragmentQueue {
  private _queue: QueuedFragment[] = [];
  private _isProcessing = false;
  private _handlers: FragmentQueueHandlers;
  // Set once the fragment marked isFinal is enqueued. Fragments now arrive as
  // each one is synthesized, so an empty queue usually means "waiting for the
  // next fragment", not "reading finished". Only the final fragment proves the
  // reading is over.
  private _finalSeen = false;

  constructor(handlers: FragmentQueueHandlers) {
    this._handlers = handlers;
  }

  /**
   * Add a fragment to the queue
   */
  enqueue(fragment: QueuedFragment): void {
    // Index 0 marks the start of a reading: clear any final marker left by the
    // previous reading so this one cannot be considered complete before its own
    // last fragment arrives. Done here (not on a separate "new reading" event)
    // so it cannot interfere with playback that is still in progress.
    if (fragment.index === 0) this._finalSeen = false;
    if (fragment.isFinal) this._finalSeen = true;
    this._queue.push(fragment);
  }

  /**
   * Get the current fragment being played
   */
  getCurrentFragment(): QueuedFragment | null {
    return this._queue.length > 0 ? this._queue[0] : null;
  }

  /**
   * Get the current fragment index (1-based)
   */
  getCurrentIndex(): number | null {
    return this._queue.length > 0 ? this._queue[0].index : null;
  }

  /**
   * Get the total number of fragments
   */
  getTotalFragments(): number | null {
    return this._queue.length > 0 ? this._queue[0].total : null;
  }

  /**
   * Check if the queue is currently processing
   */
  isProcessing(): boolean {
    return this._isProcessing;
  }

  /**
   * Start processing the queue if not already
   */
  async startProcessing(): Promise<void> {
    if (this._isProcessing) return;

    this._isProcessing = true;
    await this.processNext();
  }

  /**
   * Process the next fragment in the queue
   */
  private async processNext(): Promise<void> {
    if (this._queue.length === 0) {
      this._isProcessing = false;
      return;
    }

    const next = this._queue[0];
    await this._handlers.onFragmentPlay(next);
  }

  /**
   * Handle when current fragment playback ends
   * Auto-advances to next queued fragment or stops if queue is empty.
   */
  handleFragmentEnded(): void {
    // Remove the just-completed fragment from queue
    if (this._queue.length > 0) {
      this._queue.shift();
    }

    if (this._queue.length > 0) {
      // Auto-advance to next fragment
      void this.processNext();
    } else if (!this._finalSeen) {
      // Queue drained but the reading is not over: the next fragment is still
      // being synthesized. Stop processing (a later enqueue restarts it) and
      // wait rather than reporting completion, which would hide the HUD and
      // reset playback state mid-reading.
      this._isProcessing = false;
    } else {
      // No more fragments - playback complete
      this._isProcessing = false;
      this._handlers.onQueueComplete();
    }
  }

  /**
   * Mark the reading as terminal without waiting for a fragment. Used when
   * synthesis reports failure or the backend says the reading is complete, so
   * a starved queue can still finish instead of hanging.
   */
  markComplete(): void {
    this._finalSeen = true;
    if (this._queue.length === 0) {
      this._isProcessing = false;
      this._handlers.onQueueComplete();
    }
  }

  /**
   * Clear the queue and stop processing
   */
  clear(): void {
    this._queue = [];
    this._isProcessing = false;
    this._finalSeen = false;
  }

  /**
   * Get the number of fragments in the queue
   */
  getQueueLength(): number {
    return this._queue.length;
  }

  /**
   * Get all fragments in the queue (read-only)
   */
  getQueue(): readonly QueuedFragment[] {
    return this._queue;
  }
}
