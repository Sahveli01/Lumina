/**
 * Contract ID registry — reads deployed Soroban contract addresses from env.
 *
 * All four IDs start as empty strings; the code functions as-is once real
 * contract addresses are placed in .env (after `bash scripts/deploy.sh testnet`).
 *
 * Stellar contract ID format: starts with 'C', exactly 56 alphanumeric chars
 * in Stellar base32 alphabet (A-Z, 2-7).  Example:
 *   CBIELTK6YBZJU5UP2WWQEUCYKLPU6AUNZ2BQ4WWFEIE3USCIHMXQDAMA
 */

// Stellar contract ID: Strkey-encoded contract address — 'C' + 55 base32 chars.
const CONTRACT_ID_RE = /^C[A-Z2-7]{55}$/;

/**
 * Returns true if `id` is a validly formatted Stellar contract ID.
 * Does NOT verify on-chain existence — only checks structural validity.
 */
export function isValidContractId(id: unknown): id is string {
  return typeof id === 'string' && CONTRACT_ID_RE.test(id);
}

const ContractIds = {
  luminaCore:        process.env['LUMINA_CORE_CONTRACT_ID']          ?? '',
  nullifierRegistry: process.env['NULLIFIER_REGISTRY_CONTRACT_ID']   ?? '',
  riskOracle:        process.env['RISK_ORACLE_CONTRACT_ID']           ?? '',
  liquidityPools:    process.env['LIQUIDITY_POOLS_CONTRACT_ID']       ?? '',
} as const;

type ContractName = keyof typeof ContractIds;

const ENV_KEYS: Record<ContractName, string> = {
  luminaCore:        'LUMINA_CORE_CONTRACT_ID',
  nullifierRegistry: 'NULLIFIER_REGISTRY_CONTRACT_ID',
  riskOracle:        'RISK_ORACLE_CONTRACT_ID',
  liquidityPools:    'LIQUIDITY_POOLS_CONTRACT_ID',
};

/**
 * Returns the contract ID for `name`, throwing a descriptive error if:
 *  - the environment variable has not been set, OR
 *  - the value is not a valid Stellar contract ID (C + 55 base32 chars).
 */
export function requireContractId(name: ContractName): string {
  const id = ContractIds[name];
  if (!id) {
    throw new Error(
      `Contract "${name}" is not configured. ` +
        `Set ${ENV_KEYS[name]} in your .env file after deploying with:\n` +
        `  bash scripts/deploy.sh testnet`
    );
  }
  if (!isValidContractId(id)) {
    throw new Error(
      `Contract "${name}" has an invalid ID format: "${id}"\n` +
        `Expected: C followed by 55 base32 characters (A-Z, 2-7).\n` +
        `Re-run deployment: bash scripts/deploy.sh testnet`
    );
  }
  return id;
}

export { ContractIds, type ContractName };
