import { ethers } from "ethers";

import { Providers } from "types/providers";
import { TxSender } from "utils/tx-sender";

import { ArtifactsService } from "./ArtifactsService";
import { ConfigService } from "./ConfigService";
import { ContractsService } from "./ContractsService";
import { NetworkService } from "./NetworkService";
import { TransactionsService } from "./TransactionsService";
import { Services } from "./types";

export function createServices(providers: Providers): Services {
  const txSender = new TxSender(providers.gasProvider);

  const services: Services = {
    network: new NetworkService(providers),
    artifacts: new ArtifactsService(providers),
    contracts: new ContractsService(
      {
        gasProvider: providers.gasProvider,
        signersProvider: providers.signers,
        transactionsProvider: providers.transactions,
        web3Provider: new ethers.providers.Web3Provider(
          providers.ethereumProvider
        ),
      },
      txSender
    ),
    transactions: new TransactionsService(providers),
    config: new ConfigService(providers),
  };

  return services;
}