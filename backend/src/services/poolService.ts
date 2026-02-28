import { StellarClient } from '../stellar/client';

export interface PoolState {
  type: 'senior' | 'junior';
  totalLiquidity: string;
  availableLiquidity: string;
  deployedCapital: string;
  apy: number;
  participantCount: number;
}

export class PoolService {
  private stellar: StellarClient;

  constructor() {
    this.stellar = new StellarClient();
  }

  async listPools(): Promise<PoolState[]> {
    // TODO: Query liquidity-pools contract for both pool states
    return [
      {
        type: 'senior',
        totalLiquidity: '0',
        availableLiquidity: '0',
        deployedCapital: '0',
        apy: 0,
        participantCount: 0,
      },
      {
        type: 'junior',
        totalLiquidity: '0',
        availableLiquidity: '0',
        deployedCapital: '0',
        apy: 0,
        participantCount: 0,
      },
    ];
  }

  async getPool(type: 'senior' | 'junior'): Promise<PoolState | null> {
    const pools = await this.listPools();
    return pools.find((p) => p.type === type) ?? null;
  }

  async deposit(
    type: 'senior' | 'junior',
    opts: { amount: string; depositorPublicKey: string }
  ): Promise<{ txHash: string; lpTokens: string }> {
    // TODO: Call liquidity-pools.deposit() on Soroban
    throw new Error('Not implemented yet');
  }

  async withdraw(
    type: 'senior' | 'junior',
    opts: { lpTokens: string; withdrawerPublicKey: string }
  ): Promise<{ txHash: string; amount: string }> {
    // TODO: Call liquidity-pools.withdraw() on Soroban
    throw new Error('Not implemented yet');
  }
}
