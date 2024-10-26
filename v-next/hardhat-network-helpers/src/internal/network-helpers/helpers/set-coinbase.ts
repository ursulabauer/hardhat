import type { EthereumProvider } from "@ignored/hardhat-vnext/types/providers";

import { assertValidAddress } from "../../assertions.js";

export async function setCoinbase(
  provider: EthereumProvider,
  address: string,
): Promise<void> {
  await assertValidAddress(address);

  await provider.request({
    method: "hardhat_setCoinbase",
    params: [address],
  });
}