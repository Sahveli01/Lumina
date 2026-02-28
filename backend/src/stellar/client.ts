import * as StellarSdk from '@stellar/stellar-sdk';

const {
  Contract,
  TransactionBuilder,
  Networks,
  BASE_FEE,
  Keypair,
  Address,
  nativeToScVal,
  scValToNative,
} = StellarSdk;

export interface ContractCallOpts {
  contractId: string;
  method: string;
  args: StellarSdk.xdr.ScVal[];
  signerSecretKey?: string;
}

export class StellarClient {
  private rpc: StellarSdk.rpc.Server;
  private networkPassphrase: string;

  constructor() {
    const rpcUrl = process.env.STELLAR_RPC_URL ?? 'https://soroban-testnet.stellar.org';
    this.networkPassphrase =
      process.env.STELLAR_NETWORK_PASSPHRASE ?? Networks.TESTNET;
    this.rpc = new StellarSdk.rpc.Server(rpcUrl, { allowHttp: false });
  }

  async callContract(opts: ContractCallOpts): Promise<StellarSdk.xdr.ScVal> {
    const secretKey = opts.signerSecretKey ?? process.env.ADMIN_SECRET_KEY;
    if (!secretKey) throw new Error('No signer secret key provided');

    const keypair = Keypair.fromSecret(secretKey);
    const account = await this.rpc.getAccount(keypair.publicKey());

    const contract = new Contract(opts.contractId);
    const tx = new TransactionBuilder(account, {
      fee: BASE_FEE,
      networkPassphrase: this.networkPassphrase,
    })
      .addOperation(contract.call(opts.method, ...opts.args))
      .setTimeout(30)
      .build();

    const preparedTx = await this.rpc.prepareTransaction(tx);
    preparedTx.sign(keypair);

    const sendResp = await this.rpc.sendTransaction(preparedTx);
    if (sendResp.status === 'ERROR') {
      throw new Error(`Transaction failed: ${JSON.stringify(sendResp.errorResult)}`);
    }

    // Poll for confirmation
    let getResp = await this.rpc.getTransaction(sendResp.hash);
    while (getResp.status === 'NOT_FOUND') {
      await new Promise((r) => setTimeout(r, 1000));
      getResp = await this.rpc.getTransaction(sendResp.hash);
    }

    if (getResp.status === 'FAILED') {
      throw new Error(`Transaction FAILED: ${sendResp.hash}`);
    }

    const successResp = getResp as StellarSdk.rpc.Api.GetSuccessfulTransactionResponse;
    return successResp.returnValue ?? nativeToScVal(null);
  }

  async getContractData(
    contractId: string,
    key: StellarSdk.xdr.ScVal
  ): Promise<StellarSdk.xdr.ScVal | null> {
    try {
      const ledgerKey = StellarSdk.xdr.LedgerKey.contractData(
        new StellarSdk.xdr.LedgerKeyContractData({
          contract: new Address(contractId).toScAddress(),
          key,
          durability: StellarSdk.xdr.ContractDataDurability.persistent(),
        })
      );
      const resp = await this.rpc.getLedgerEntries(ledgerKey);
      if (resp.entries.length === 0) return null;
      const entry = resp.entries[0].val.contractData();
      return entry.val();
    } catch {
      return null;
    }
  }

  // Helper: encode string to ScVal symbol
  symbol(s: string): StellarSdk.xdr.ScVal {
    return StellarSdk.xdr.ScVal.scvSymbol(s);
  }

  // Helper: encode address to ScVal
  address(addr: string): StellarSdk.xdr.ScVal {
    return new Address(addr).toScVal();
  }

  // Helper: encode i128 amount
  i128(value: bigint): StellarSdk.xdr.ScVal {
    return nativeToScVal(value, { type: 'i128' });
  }
}
