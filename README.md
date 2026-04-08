# Matrix Burn Bot

This bot automatically deletes all user messages within a specific Matrix room after a specific time.  
The user that is configured for this bot needs to have the right to delete messages or it will fail.  

The Matrix spec and ecosystem has curently no native way for self-disappearing messages.  
In more mainstream messager apps this is a common feature and it helps with data hygiene. 
However, there is on-going work around bringen this feature to Matrix, which well make this bot eventually obsolete.  

The bot is written in Rust and uses the official matrix-rust-sdk for handling everthing essential.  

This bot is inspired by [this burn bot](https://github.com/syncpoint/burn-bot) written in JavaScript. I have used this bot myself for some time, but it is not actively maintained anymore and has one huge disadvantage: The session can not be verified. So the account that is used for the bot causes a "not trusted" warning in Matrix clients and will probably stop working altogether in encrypted rooms when Matrix stops sending encrypted messages to unverified sessions.


## Resources

  * https://github.com/matrix-org/matrix-spec-proposals/pull/4140
  * https://matrix-org.github.io/synapse/v1.98/message_retention_policies.html (spec proposal only, experimental, depending on homeserver and homeserver configuration)
  * https://github.com/element-hq/element-meta/discussions/712
  * https://github.com/element-hq/element-meta/discussions/682


## License

This project is licensed under the terms of the [MIT license](LICENSE).
