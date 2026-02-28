import { StellarClient } from '../stellar/client';

export interface Invoice {
  id: string;
  companyId: string;
  debtorId: string;
  amount: number;
  currency: string;
  dueDate: string;
  status: 'pending' | 'factored' | 'repaid' | 'defaulted';
  nullifierHash?: string;
  proofId?: string;
  createdAt: string;
}

export class InvoiceService {
  private stellar: StellarClient;

  constructor() {
    this.stellar = new StellarClient();
  }

  async listInvoices(companyId?: string, status?: string): Promise<Invoice[]> {
    // TODO: Fetch from contract storage via Stellar RPC
    return [];
  }

  async getInvoice(id: string): Promise<Invoice | null> {
    // TODO: Query lumina-core contract for invoice data
    return null;
  }

  async submitInvoice(data: Partial<Invoice>): Promise<Invoice> {
    if (!data.amount || !data.debtorId || !data.dueDate) {
      throw new Error('Missing required invoice fields: amount, debtorId, dueDate');
    }

    const invoice: Invoice = {
      id: `inv_${Date.now()}`,
      companyId: data.companyId ?? 'unknown',
      debtorId: data.debtorId,
      amount: data.amount,
      currency: data.currency ?? 'USDC',
      dueDate: data.dueDate,
      status: 'pending',
      createdAt: new Date().toISOString(),
    };

    // TODO: Submit to lumina-core Soroban contract
    return invoice;
  }

  async factorInvoice(
    invoiceId: string,
    opts: { proofBytes: string; nullifierHash: string }
  ): Promise<{ txHash: string; advanceAmount: number }> {
    // TODO:
    // 1. Verify ZK proof via proof service
    // 2. Register nullifier in nullifier-registry contract
    // 3. Call lumina-core.factor_invoice() on-chain
    // 4. Return tx hash + advance amount
    throw new Error('Not implemented yet');
  }
}
